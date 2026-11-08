use agent_client_protocol::v1::schema as acp;

use crate::AgentCommand;

/// The command to run for a `type: "terminal"` auth method: the configured
/// agent invocation with the method's args appended and env overlaid
/// (spec: authentication.mdx, "Terminal Authentication").
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalAuthCommand {
    pub method_id: String,
    pub method_name: String,
    pub command: AgentCommand,
    /// Env names the user must confirm before they are applied.
    pub sensitive_env: Vec<String>,
}

impl TerminalAuthCommand {
    pub fn new(agent: &AgentCommand, method: &acp::AuthMethodTerminal) -> Self {
        let mut command = agent.clone();
        command.args.extend(method.args.iter().cloned());
        let mut overlay: Vec<(String, String)> = method
            .env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        overlay.sort();
        let sensitive_env = overlay
            .iter()
            .filter(|(name, _)| is_sensitive_env_var(name))
            .map(|(name, _)| name.clone())
            .collect();
        command.env.extend(overlay);
        Self {
            method_id: method.id.to_string(),
            method_name: method.name.clone(),
            command,
            sensitive_env,
        }
    }
}

/// Environment variables that can change which code runs or where traffic
/// goes. The spec asks clients to confirm these before applying them.
pub fn is_sensitive_env_var(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "PATH",
        "NODE_OPTIONS",
        "NODE_PATH",
        "PYTHONPATH",
        "PYTHONSTARTUP",
        "RUBYOPT",
        "PERL5OPT",
        "BASH_ENV",
        "ENV",
        "SHELL",
        "HOME",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NODE_EXTRA_CA_CERTS",
        "REQUESTS_CA_BUNDLE",
    ];
    let upper = name.to_ascii_uppercase();
    EXACT.contains(&upper.as_str())
        || upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
        || upper.ends_with("_PROXY")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    #[test]
    fn terminal_auth_appends_args_and_flags_sensitive_env() {
        let agent = AgentCommand {
            command: "agent".to_string(),
            args: vec!["acp".to_string()],
            ..AgentCommand::default()
        };
        let method = acp::AuthMethodTerminal::new("login", "Log in")
            .args(vec!["--login".to_string()])
            .env(HashMap::from([
                ("LD_PRELOAD".to_string(), "x.so".to_string()),
                ("AGENT_MODE".to_string(), "login".to_string()),
            ]));
        let auth = TerminalAuthCommand::new(&agent, &method);
        assert_eq!(auth.command.args, vec!["acp", "--login"]);
        assert_eq!(
            auth.command.env,
            vec![
                ("AGENT_MODE".to_string(), "login".to_string()),
                ("LD_PRELOAD".to_string(), "x.so".to_string()),
            ]
        );
        assert_eq!(auth.sensitive_env, vec!["LD_PRELOAD".to_string()]);
    }
}
