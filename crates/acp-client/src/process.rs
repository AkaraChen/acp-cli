use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;

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
    pub child: Child,
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
    let mut child = command.spawn()?;
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
        child,
        stdin,
        stdout,
        stderr: tail,
    })
}

/// Ask the agent's process group to exit, then kill it if it lingers.
pub(crate) async fn terminate(mut child: Child, grace: std::time::Duration) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let pgid = libc::pid_t::try_from(pid).unwrap_or(0);
        if pgid > 0 {
            // SAFETY: signalling our own child's process group.
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
        }
    }
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            let pgid = libc::pid_t::try_from(pid).unwrap_or(0);
            if pgid > 0 {
                // SAFETY: signalling our own child's process group.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
        let _ = child.kill().await;
    }
}
