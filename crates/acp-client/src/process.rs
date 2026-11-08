use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::time::Instant;

/// How to launch an agent subprocess.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentCommand {
    pub command: String,
    pub args: Vec<String>,
    /// Variables set on the child, applied after `env_remove`.
    pub env: Vec<(String, String)>,
    /// Variables removed from the inherited environment.
    pub env_remove: Vec<String>,
    pub cwd: Option<PathBuf>,
}

impl AgentCommand {
    pub fn display(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn to_command(&self) -> Command {
        let mut command = Command::new(&self.command);
        command.args(&self.args);
        for name in &self.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &self.env {
            command.env(name, value);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command
    }
}

const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// The last bytes an agent wrote to stderr, kept for error reports.
#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        let mut lines = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lines.push_back(line);
        let mut total: usize = lines.iter().map(|line| line.len() + 1).sum();
        while total > STDERR_TAIL_BYTES {
            let Some(dropped) = lines.pop_front() else {
                break;
            };
            total -= dropped.len() + 1;
        }
    }

    pub(crate) fn snapshot(&self) -> String {
        let lines = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lines.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

pub(crate) struct SpawnedAgent {
    pub process: AgentProcess,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: StderrTail,
}

pub(crate) fn spawn_agent(spec: &AgentCommand) -> std::io::Result<SpawnedAgent> {
    let mut command = spec.to_command();
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        // Own process group so shutdown can signal the agent's children too.
        command.process_group(0);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe and touches no shared state.
    unsafe {
        command.pre_exec(|| {
            // Do not outlive acpc if it is killed without running destructors.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = AgentProcess::new(command.spawn()?);
    let child = &mut process.child;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(std::io::Error::other("agent stdio was not piped"));
    };
    let tail = StderrTail::default();
    let tail_writer = tail.clone();
    let agent_name = spec.command.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "acp_client::agent_stderr", agent = %agent_name, "{line}");
            tail_writer.push(line);
        }
    });
    Ok(SpawnedAgent {
        process,
        stdin,
        stdout,
        stderr: tail,
    })
}

/// The agent process, owned from spawn. The agent leads its own process
/// group; dropping this kills the whole group unless [`Self::terminate`]
/// already ended it. The leader stays unreaped until then, which keeps the
/// group id from being reused.
pub(crate) struct AgentProcess {
    child: Child,
    /// Cleared once the group has been ended on purpose.
    pgid: Option<i32>,
    reaped: bool,
}

impl AgentProcess {
    fn new(child: Child) -> Self {
        let pgid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pgid| *pgid > 0);
        Self {
            child,
            pgid,
            reaped: false,
        }
    }

    /// Wait up to `timeout` for the leader to exit by itself.
    pub(crate) async fn wait(&mut self, timeout: Duration) {
        if !self.reaped
            && tokio::time::timeout(timeout, self.child.wait())
                .await
                .is_ok()
        {
            self.reaped = true;
        }
    }

    /// Ask the whole group to exit, give it `grace`, then kill what is left.
    pub(crate) async fn terminate(&mut self, grace: Duration) {
        let Some(pgid) = self.pgid else {
            return;
        };
        if self.group_alive(pgid) {
            signal_group(pgid, Signal::Term);
        }
        let deadline = Instant::now() + grace;
        loop {
            if !self.reaped && !matches!(self.child.try_wait(), Ok(None)) {
                self.reaped = true;
            }
            if !self.group_alive(pgid) {
                break;
            }
            if Instant::now() >= deadline {
                signal_group(pgid, Signal::Kill);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Cancelled before this point, Drop still kills the group.
        self.pgid = None;
        if !self.reaped {
            let _ = self.child.kill().await;
            self.reaped = true;
        }
    }

    /// An unreaped leader pins the group id, so signalling it is safe;
    /// afterwards the group lives only as long as some member does.
    fn group_alive(&self, pgid: i32) -> bool {
        !self.reaped || signal_group(pgid, Signal::Probe)
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid
            && self.group_alive(pgid)
        {
            signal_group(pgid, Signal::Kill);
        }
    }
}

enum Signal {
    Probe,
    Term,
    Kill,
}

/// Returns whether the group exists (and may be signalled).
fn signal_group(pgid: i32, signal: Signal) -> bool {
    #[cfg(unix)]
    {
        let signal = match signal {
            Signal::Probe => 0,
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: `pgid` is positive and names the group acpc created for
        // the agent; a negative pid addresses the whole group.
        unsafe { libc::kill(-pgid, signal) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, signal);
        false
    }
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    /// Start `sh -c script`; the script prints the pid of a background child.
    async fn spawn_with_child(script: &str) -> (AgentProcess, i32) {
        let spec = AgentCommand {
            command: "sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            ..Default::default()
        };
        let spawned = spawn_agent(&spec).expect("spawn sh");
        let mut line = String::new();
        BufReader::new(spawned.stdout)
            .read_line(&mut line)
            .await
            .expect("read pid");
        (spawned.process, line.trim().parse().expect("pid"))
    }

    /// Gone or a zombie waiting for its new parent to reap it.
    async fn dies(pid: i32) -> bool {
        for _ in 0..40 {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat)
                    if stat
                        .rsplit(')')
                        .next()
                        .is_some_and(|rest| rest.trim_start().starts_with('Z')) =>
                {
                    return true;
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        false
    }

    #[tokio::test]
    async fn drop_kills_the_whole_group() {
        let (process, child) = spawn_with_child("sleep 30 & echo $!; wait").await;
        drop(process);
        assert!(dies(child).await);
    }

    #[tokio::test]
    async fn terminate_kills_children_that_ignore_sigterm() {
        let (mut process, child) = spawn_with_child("trap '' TERM; sleep 30 & echo $!; wait").await;
        process.terminate(Duration::from_millis(200)).await;
        assert!(dies(child).await);
    }

    #[tokio::test]
    async fn terminate_reaches_children_after_the_leader_exited() {
        let (mut process, child) = spawn_with_child("sleep 30 & echo $!").await;
        process.wait(Duration::from_secs(5)).await;
        let started = Instant::now();
        process.terminate(Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(dies(child).await);
    }
}
