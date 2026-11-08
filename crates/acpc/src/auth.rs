//! Authentication before the TUI owns the terminal (authentication.mdx).
//!
//! acpc creates the first session up front (`acpc resume` lists sessions
//! instead). If the agent answers `auth_required`, the user picks one of the
//! advertised methods on the plain terminal:
//! - `agent` methods: `authenticate`, then retry;
//! - `terminal` methods: run the agent's own command with the method's args
//!   and env interactively, then reconnect, re-`initialize` and retry.

use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acp_bridge::AgentProfile;
use acp_client::AgentConnection;
use acp_client::AgentEvent;
use acp_client::ClientOptions;
use acp_client::ConnectError;
use acp_client::TerminalAuthCommand;
use acp_client::schema as acp;
use anyhow::Context;
use anyhow::bail;
use tokio::sync::mpsc;

/// How long acpc waits quietly before saying what it is waiting for.
const SLOW_NOTICE: Duration = Duration::from_secs(15);

pub struct Connected {
    pub connection: Arc<AgentConnection>,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    pub session: Option<acp::NewSessionResponse>,
}

/// Connect, create the first session (or, without one, list sessions in
/// `cwd`), and authenticate if the agent asks.
pub async fn connect_with_auth(
    profile: &AgentProfile,
    client: &ClientOptions,
    cwd: &Path,
    new_session: Option<acp::NewSessionRequest>,
    force_login: bool,
) -> anyhow::Result<Connected> {
    let (connection, events) = connect(profile, client)
        .await
        .with_context(|| format!("could not start agent `{}`", profile.slug))?;
    let mut held = Vec::new();
    let result = sign_in_and_open(profile, client, cwd, connection, events, new_session, force_login, &mut held).await;
    result.map(|mut connected| {
        connected.events = replay_held(held, connected.events);
        connected
    })
}

/// [`AgentConnection::connect`], saying so when the agent is slow to start.
async fn connect(profile: &AgentProfile, client: &ClientOptions) -> Result<Pair, ConnectError> {
    let connecting = AgentConnection::connect(profile.command.clone(), client);
    tokio::pin!(connecting);
    tokio::select! {
        result = &mut connecting => return result,
        () = tokio::time::sleep(SLOW_NOTICE) => {
            eprintln!("Still waiting for {} to start ({})…", profile.name, profile.command.display());
        }
    }
    connecting.await
}

/// Events seen while acpc waited stay queued for the bridge.
fn replay_held(
    held: Vec<AgentEvent>,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
) -> mpsc::UnboundedReceiver<AgentEvent> {
    if held.is_empty() {
        return events;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    for event in held {
        let _ = tx.send(event);
    }
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if tx.send(event).is_err() {
                break;
            }
        }
    });
    rx
}

/// Await `work` while answering what the agent asks meanwhile: sign-in
/// forms are shown on the terminal, permissions are cancelled (no turn is
/// running), anything else is held for the bridge. Says once what it is
/// `waiting` for if the agent takes a while.
async fn answering<T>(
    waiting: &str,
    work: impl std::future::Future<Output = T>,
    connection: &AgentConnection,
    events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    held: &mut Vec<AgentEvent>,
) -> T {
    tokio::pin!(work);
    let slow = tokio::time::sleep(SLOW_NOTICE);
    tokio::pin!(slow);
    let mut noted = false;
    loop {
        tokio::select! {
            result = &mut work => return result,
            () = &mut slow, if !noted => {
                noted = true;
                eprintln!("Still waiting for {waiting}…{}", stderr_note(connection));
            }
            Some(event) = events.recv() => match event {
                AgentEvent::PermissionRequested { id, .. } => {
                    connection.respond_permission(id, acp::RequestPermissionOutcome::Cancelled);
                }
                AgentEvent::ElicitationRequested { id, request } => {
                    let response = crate::prompt::elicit(request).await;
                    connection.respond_elicitation(id, response);
                    // Time the user spent on the form is not the agent's.
                    slow.as_mut().reset(tokio::time::Instant::now() + SLOW_NOTICE);
                }
                other => held.push(other),
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sign_in_and_open(
    profile: &AgentProfile,
    client: &ClientOptions,
    cwd: &Path,
    mut connection: Arc<AgentConnection>,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    new_session: Option<acp::NewSessionRequest>,
    force_login: bool,
    held: &mut Vec<AgentEvent>,
) -> anyhow::Result<Connected> {
    let mut attempted = force_login;
    if force_login {
        (connection, events) = authenticate(profile, client, connection, events, held).await?;
    }
    let Some(new_session) = new_session else {
        // `acpc resume` opens no session here; listing them is where the
        // agent says whether it needs a sign-in first.
        if !force_login && connection.caps().list_sessions() {
            (connection, events) = probe_sessions(profile, client, cwd, connection, events, held).await?;
        }
        return Ok(Connected {
            connection,
            events,
            session: None,
        });
    };
    loop {
        let request = new_session.clone().mcp_servers(
            connection
                .caps()
                .filter_mcp_servers(new_session.mcp_servers.clone()),
        );
        let waiting = format!("{} to create a session", profile.name);
        match answering(&waiting, connection.new_session(request), &connection, &mut events, held).await {
            Ok(session) => {
                return Ok(Connected {
                    connection,
                    events,
                    session: Some(session),
                });
            }
            Err(err) if err.is_auth_required() && !attempted => {
                attempted = true;
                eprintln!("{} needs you to sign in.", profile.name);
                (connection, events) =
                    authenticate(profile, client, connection, events, held).await?;
            }
            Err(err) if err.is_auth_required() => {
                bail!("{} still requires authentication: {}", profile.name, err.message)
            }
            Err(err) => bail!(
                "{} could not create a session: {}{}",
                profile.name,
                err.message,
                stderr_note(&connection)
            ),
        }
    }
}

async fn probe_sessions(
    profile: &AgentProfile,
    client: &ClientOptions,
    cwd: &Path,
    mut connection: Arc<AgentConnection>,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    held: &mut Vec<AgentEvent>,
) -> anyhow::Result<Pair> {
    let mut attempted = false;
    loop {
        let request = acp::ListSessionsRequest::new().cwd(cwd.to_path_buf());
        let waiting = format!("{} to list sessions", profile.name);
        match answering(&waiting, connection.list_sessions(request), &connection, &mut events, held).await {
            Ok(_) => return Ok((connection, events)),
            Err(err) if err.is_auth_required() && !attempted => {
                attempted = true;
                eprintln!("{} needs you to sign in.", profile.name);
                (connection, events) =
                    authenticate(profile, client, connection, events, held).await?;
            }
            Err(err) if err.is_auth_required() => {
                bail!("{} still requires authentication: {}", profile.name, err.message)
            }
            // The TUI lists sessions itself and reports other failures.
            Err(err) => {
                tracing::warn!("session/list before resuming failed: {}", err.message);
                return Ok((connection, events));
            }
        }
    }
}

type Pair = (Arc<AgentConnection>, mpsc::UnboundedReceiver<AgentEvent>);

fn stderr_note(connection: &AgentConnection) -> String {
    let stderr = connection.stderr_tail();
    let lines: Vec<&str> = stderr.lines().collect();
    let tail = &lines[lines.len().saturating_sub(5)..];
    if tail.is_empty() {
        String::new()
    } else {
        format!("\nagent stderr:\n{}", tail.join("\n"))
    }
}

async fn authenticate(
    profile: &AgentProfile,
    client: &ClientOptions,
    connection: Arc<AgentConnection>,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    held: &mut Vec<AgentEvent>,
) -> anyhow::Result<Pair> {
    let methods = connection.caps().auth_methods().to_vec();
    if methods.is_empty() {
        bail!(
            "{} requires authentication but advertises no sign-in methods; sign in with the agent's own tools",
            profile.name
        );
    }
    let method = choose_method(&methods)?;
    match method {
        acp::AuthMethod::Terminal(terminal) => {
            if !client.terminal_auth {
                bail!("terminal sign-in needs an interactive terminal");
            }
            let auth = TerminalAuthCommand::new(&profile.command, &terminal);
            connection.shutdown().await;
            drop(events);
            // The old process's events end with it.
            held.clear();
            run_terminal_login(&auth).await?;
            connect(profile, client)
                .await
                .with_context(|| format!("could not restart agent `{}`", profile.slug))
        }
        acp::AuthMethod::Agent(agent) => {
            eprintln!("Signing in with {}…", agent.name);
            let waiting = format!("{} to finish signing in", profile.name);
            answering(&waiting, connection.authenticate(agent.id.clone()), &connection, &mut events, held)
                .await
                .map_err(|err| anyhow::anyhow!("sign-in failed: {}{}", err.message, stderr_note(&connection)))?;
            Ok((connection, events))
        }
        _ => bail!("unsupported sign-in method"),
    }
}

fn method_label(method: &acp::AuthMethod) -> (String, Option<String>) {
    match method {
        acp::AuthMethod::Agent(agent) => (agent.name.clone(), agent.description.clone()),
        acp::AuthMethod::Terminal(terminal) => (
            format!("{} (opens in this terminal)", terminal.name),
            terminal.description.clone(),
        ),
        _ => ("unknown method".to_string(), None),
    }
}

fn choose_method(methods: &[acp::AuthMethod]) -> anyhow::Result<acp::AuthMethod> {
    let supported: Vec<&acp::AuthMethod> = methods
        .iter()
        .filter(|method| matches!(method, acp::AuthMethod::Agent(_) | acp::AuthMethod::Terminal(_)))
        .collect();
    match supported.as_slice() {
        [] => bail!("the agent offers no sign-in method acpc supports"),
        [only] => Ok((*only).clone()),
        many => {
            eprintln!("Choose how to sign in:");
            for (index, method) in many.iter().enumerate() {
                let (label, description) = method_label(method);
                match description {
                    Some(description) => eprintln!("  {}. {label} — {description}", index + 1),
                    None => eprintln!("  {}. {label}", index + 1),
                }
            }
            let choice = prompt_line(&format!("Method [1-{}]: ", many.len()))?;
            let index = choice
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|index| (1..=many.len()).contains(index))
                .context("invalid choice")?;
            Ok(many[index - 1].clone())
        }
    }
}

fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}

/// Run a terminal auth command with the user's terminal, after showing what
/// will run and confirming sensitive environment overrides.
pub async fn run_terminal_login(auth: &TerminalAuthCommand) -> anyhow::Result<()> {
    let display = escaped(&auth.command.display());
    eprintln!("Running: {display}");
    let overlay: Vec<&(String, String)> = auth
        .command
        .env
        .iter()
        .filter(|(name, _)| auth.sensitive_env.contains(name))
        .collect();
    if !overlay.is_empty() {
        eprintln!("The agent asks to set these environment variables:");
        for (name, value) in &overlay {
            eprintln!("  {}={}", escaped(name), escaped(value));
        }
        let answer = prompt_line("Allow? [y/N] ")?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            bail!("sign-in cancelled");
        }
    }
    let mut command = tokio::process::Command::new(&auth.command.command);
    command.args(&auth.command.args);
    for name in &auth.command.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &auth.command.env {
        command.env(name, value);
    }
    if let Some(cwd) = &auth.command.cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("running {display}"))?;
    // Ctrl+C and Ctrl+\ reach the whole foreground group: let the sign-in
    // program handle them instead of killing acpc.
    let status = {
        let _ignored = IgnoreTerminalSignals::new();
        child.wait().await?
    };
    if !status.success() {
        bail!("sign-in command exited with {status}");
    }
    eprintln!("Signed in.");
    Ok(())
}

/// Ignores SIGINT and SIGQUIT in acpc until dropped, then restores the
/// previous dispositions. A child spawned before keeps the default ones.
struct IgnoreTerminalSignals {
    #[cfg(unix)]
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl IgnoreTerminalSignals {
    fn new() -> Self {
        #[cfg(unix)]
        {
            let previous = [libc::SIGINT, libc::SIGQUIT]
                .into_iter()
                .filter_map(|signal| {
                    // SAFETY: plain sigaction calls on zero-initialised structs.
                    unsafe {
                        let mut ignore: libc::sigaction = std::mem::zeroed();
                        ignore.sa_sigaction = libc::SIG_IGN;
                        libc::sigemptyset(&mut ignore.sa_mask);
                        let mut previous: libc::sigaction = std::mem::zeroed();
                        (libc::sigaction(signal, &ignore, &mut previous) == 0).then_some((signal, previous))
                    }
                })
                .collect();
            Self { previous }
        }
        #[cfg(not(unix))]
        Self {}
    }
}

impl Drop for IgnoreTerminalSignals {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (signal, previous) in &self.previous {
            // SAFETY: restores the disposition saved in `new`.
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

/// Agent-supplied text with control characters made visible, so it cannot
/// rewrite what the terminal shows.
fn escaped(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
            shown.extend(c.escape_default());
        } else {
            shown.push(c);
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn escapes_control_characters_from_the_agent() {
        assert_eq!(
            escaped("/tmp/evil:\u{1b}[2K\r  PATH=/usr/bin\u{202e}"),
            "/tmp/evil:\\u{1b}[2K\\r  PATH=/usr/bin\\u{202e}"
        );
        assert_eq!(escaped("npx -y agent@1 --ünïcode"), "npx -y agent@1 --ünïcode");
    }
}
