//! Terminal authentication: when launched with a terminal auth method's
//! arguments, the agent runs an interactive login instead of speaking ACP.

use std::io::BufRead;
use std::io::Write;
use std::process::ExitCode;

use agent_client_protocol::v1::schema::AuthMethod;
use agent_client_protocol::v1::schema::AuthMethodTerminal;

use crate::scenario::Scenario;
use crate::store::STATE_ENV;
use crate::store::StateDir;

/// The terminal method selected by the process arguments (any of its `args`
/// present) or, for a method without args, by its `env` all being set.
pub fn requested_method<'a>(
    scenario: &'a Scenario,
    args: &[String],
) -> Option<&'a AuthMethodTerminal> {
    scenario.auth_methods.iter().find_map(|method| {
        let AuthMethod::Terminal(terminal) = method else {
            return None;
        };
        let by_args = terminal.args.iter().any(|arg| args.contains(arg));
        let by_env = terminal.args.is_empty()
            && !terminal.env.is_empty()
            && terminal
                .env
                .iter()
                .all(|(key, value)| std::env::var(key).is_ok_and(|actual| actual == *value));
        (by_args || by_env).then_some(terminal)
    })
}

/// Prompts on stdout, waits for a line on stdin, then records the login.
///
/// Fails without prompting when there is no state directory: the login
/// could not reach the agent processes started after it.
pub fn run(state: &StateDir, method: &AuthMethodTerminal) -> ExitCode {
    if state.root().is_none() {
        log!(
            "login failed: no state directory to record it in \
             (set {STATE_ENV} or `state_dir` in the scenario)"
        );
        return ExitCode::FAILURE;
    }
    let mut stdout = std::io::stdout();
    let prompted = writeln!(
        stdout,
        "Mock login ({}): press Enter to authenticate",
        method.name
    )
    .and_then(|()| stdout.flush());
    if let Err(err) = prompted {
        log!("login failed: {err}");
        return ExitCode::FAILURE;
    }
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(0) => {
            log!("login aborted: stdin closed");
            return ExitCode::FAILURE;
        }
        Ok(_) => {}
        Err(err) => {
            log!("login failed: {err}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(err) = state.set_authenticated(true) {
        log!("login failed: {err}");
        return ExitCode::FAILURE;
    }
    if let Err(err) = writeln!(stdout, "Logged in.").and_then(|()| stdout.flush()) {
        log!("login succeeded but stdout failed: {err}");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_terminal_method_by_argument() {
        let scenario = Scenario::parse(
            r#"{"auth_methods": [
                {"id": "agent", "name": "Agent"},
                {"type": "terminal", "id": "tlogin", "name": "Terminal", "args": ["--login"]}
            ]}"#,
        )
        .expect("parse");
        let found = requested_method(&scenario, &["--login".to_string()]).expect("method");
        assert_eq!(found.id.to_string(), "tlogin");
        assert!(requested_method(&scenario, &[]).is_none());
        assert!(requested_method(&scenario, &["--other".to_string()]).is_none());
    }
}
