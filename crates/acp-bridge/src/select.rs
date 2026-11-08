//! `acpc/agent/select`: switch the agent new threads use, signing in first
//! if the agent asks (authentication.mdx).
//!
//! The switch creates the new agent's first session up front, so an
//! `auth_required` answer surfaces here, where the TUI is free to show the
//! method picker (`acpc/auth/select`) and hand over its terminal
//! (`acpc/terminal/run`). The next `thread/start` adopts that session.

use std::collections::BTreeMap;
use std::sync::Arc;

use acp_client::AgentConnection;
use acp_client::TerminalAuthCommand;
use acp_client::schema as acp;
use codex_app_server_protocol::AcpcAgentSelectResponse;
use codex_app_server_protocol::AcpcAuthMethod;
use codex_app_server_protocol::AcpcAuthSelectParams;
use codex_app_server_protocol::AcpcAuthSelectResponse;
use codex_app_server_protocol::AcpcTerminalRunParams;
use codex_app_server_protocol::AcpcTerminalRunResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_transport::ConnectionId;

use crate::server::Bridge;
use crate::server::INTERNAL_ERROR;
use crate::server::INVALID_PARAMS;

/// Sign-in attempts before giving up on an agent that keeps asking.
const MAX_AUTH_ATTEMPTS: u32 = 2;

#[derive(Clone)]
struct Reply {
    conn: ConnectionId,
    id: RequestId,
}

impl Reply {
    fn error(&self, bridge: &Bridge, message: impl Into<String>) {
        bridge.respond_error(self.conn, self.id.clone(), INTERNAL_ERROR, message);
    }
}

pub(crate) fn select(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, slug: String) {
    if bridge.agents.profile(&slug).is_none() {
        bridge.respond_error(conn, id, INVALID_PARAMS, format!("unknown agent `{slug}`"));
        return;
    }
    probe(bridge, slug, Reply { conn, id }, 0);
}

/// Connect and create the agent's first session.
fn probe(bridge: &mut Bridge, slug: String, reply: Reply, attempts: u32) {
    let mcp_servers = bridge.options.mcp_servers.clone();
    let cwd = bridge.options.default_cwd.clone();
    crate::agents::with_agent(
        bridge,
        &slug.clone(),
        Box::new(move |bridge, connection| {
            let connection = match connection {
                Ok(connection) => connection,
                Err(err) => {
                    reply.error(bridge, format!("could not start agent `{slug}`: {err}"));
                    return;
                }
            };
            let request = acp::NewSessionRequest::new(cwd.as_path().to_path_buf())
                .mcp_servers(connection.caps().filter_mcp_servers(mcp_servers));
            bridge.spawn(async move {
                let result = connection.new_session(request).await;
                move |bridge: &mut Bridge| match result {
                    // The session died with its process.
                    Ok(_) if !bridge.agents.is_live(&slug, &connection) => {
                        reply.error(bridge, format!("agent `{slug}` exited; try again"));
                    }
                    Ok(session) => {
                        bridge.agents.set_prepared(&slug, cwd, session);
                        bridge.agents.current = slug;
                        bridge.respond(reply.conn, reply.id.clone(), AcpcAgentSelectResponse {});
                        crate::ext::agents_updated(bridge);
                    }
                    Err(err) if err.is_auth_required() && attempts < MAX_AUTH_ATTEMPTS => {
                        sign_in(bridge, slug, connection, reply, attempts);
                    }
                    Err(err) => reply.error(
                        bridge,
                        format!("agent `{slug}` could not create a session: {}", err.message),
                    ),
                }
            });
        }),
    );
}

fn auth_methods(connection: &AgentConnection) -> Vec<AcpcAuthMethod> {
    connection
        .caps()
        .auth_methods()
        .iter()
        .filter_map(|method| match method {
            acp::AuthMethod::Agent(agent) => Some(AcpcAuthMethod {
                id: agent.id.to_string(),
                name: agent.name.clone(),
                description: agent.description.clone(),
                terminal: false,
            }),
            acp::AuthMethod::Terminal(terminal) => Some(AcpcAuthMethod {
                id: terminal.id.to_string(),
                name: terminal.name.clone(),
                description: terminal.description.clone(),
                terminal: true,
            }),
            _ => None,
        })
        .collect()
}

/// Ask the user for a sign-in method, run it, then probe again.
fn sign_in(
    bridge: &mut Bridge,
    slug: String,
    connection: Arc<AgentConnection>,
    reply: Reply,
    attempts: u32,
) {
    let methods = auth_methods(&connection);
    let agent_name = bridge
        .agents
        .profile(&slug)
        .map_or_else(|| slug.clone(), |profile| profile.name.clone());
    if methods.is_empty() {
        reply.error(
            bridge,
            format!("{agent_name} requires sign-in but offers no ACP sign-in method; sign in with its own tools"),
        );
        return;
    }
    let params = AcpcAuthSelectParams { agent_name, methods };
    bridge.request(
        None,
        |request_id| ServerRequest::AcpcAuthSelect { request_id, params },
        Box::new(move |bridge, result| {
            let method_id = result
                .ok()
                .and_then(|value| serde_json::from_value::<AcpcAuthSelectResponse>(value).ok())
                .and_then(|response| response.method_id);
            let Some(method_id) = method_id else {
                reply.error(bridge, "sign-in cancelled");
                return;
            };
            let method = connection
                .caps()
                .auth_methods()
                .iter()
                .find(|method| auth_method_id(method) == Some(method_id.as_str()))
                .cloned();
            match method {
                Some(acp::AuthMethod::Agent(agent)) => {
                    bridge.spawn(async move {
                        let result = connection.authenticate(agent.id.clone()).await;
                        move |bridge: &mut Bridge| match result {
                            Ok(()) => probe(bridge, slug, reply, attempts + 1),
                            Err(err) => reply.error(bridge, format!("sign-in failed: {}", err.message)),
                        }
                    });
                }
                Some(acp::AuthMethod::Terminal(terminal)) => {
                    terminal_sign_in(bridge, slug, &terminal, reply, attempts);
                }
                _ => reply.error(bridge, format!("unknown sign-in method `{method_id}`")),
            }
        }),
    );
}

fn auth_method_id(method: &acp::AuthMethod) -> Option<&str> {
    match method {
        acp::AuthMethod::Agent(agent) => Some(&agent.id.0),
        acp::AuthMethod::Terminal(terminal) => Some(&terminal.id.0),
        _ => None,
    }
}

/// Terminal sign-in: the client runs the agent's command with the method's
/// args and env interactively, then reconnects and re-initializes.
fn terminal_sign_in(
    bridge: &mut Bridge,
    slug: String,
    method: &acp::AuthMethodTerminal,
    reply: Reply,
    attempts: u32,
) {
    if !bridge.options.client.terminal_auth {
        reply.error(bridge, "terminal sign-in is not available in this client");
        return;
    }
    let Some(profile) = bridge.agents.profile(&slug).cloned() else {
        reply.error(bridge, format!("unknown agent `{slug}`"));
        return;
    };
    let auth = TerminalAuthCommand::new(&profile.command, method);
    let mut env: BTreeMap<String, Option<String>> = auth
        .command
        .env_remove
        .iter()
        .map(|name| (name.clone(), None))
        .collect();
    for (name, value) in &auth.command.env {
        env.insert(name.clone(), Some(value.clone()));
    }
    let title = format!("Sign in to {}: {}", profile.name, printable(&auth.method_name));
    // authentication.mdx: confirm risky environment overrides first.
    let confirm = (!auth.sensitive_env.is_empty()).then(|| {
        let overrides: Vec<String> = auth
            .command
            .env
            .iter()
            .filter(|(name, _)| auth.sensitive_env.contains(name))
            .map(|(name, value)| format!("  {}={}", printable(name), printable(value)))
            .collect();
        format!(
            "The agent asks to set these environment variables for its sign-in command:\n{}",
            overrides.join("\n")
        )
    });
    let params = AcpcTerminalRunParams {
        title,
        confirm,
        command: auth.command.command.clone(),
        args: auth.command.args.clone(),
        env,
        cwd: auth
            .command
            .cwd
            .as_ref()
            .map(|cwd| cwd.display().to_string()),
    };
    bridge.request(
        None,
        |request_id| ServerRequest::AcpcTerminalRun { request_id, params },
        Box::new(move |bridge, result| {
            let exit_code = result
                .ok()
                .and_then(|value| serde_json::from_value::<AcpcTerminalRunResponse>(value).ok())
                .and_then(|response| response.exit_code);
            if exit_code != Some(0) {
                reply.error(bridge, "sign-in did not complete");
                return;
            }
            // The agent must be restarted and re-initialized after terminal
            // sign-in (authentication.mdx).
            // Its sessions end with the old process.
            crate::threads::unload_agent_threads(
                bridge,
                &slug,
                "The agent restarted to finish signing in; use /resume to reopen its sessions.",
            );
            let restarted = bridge.agents.restart(&slug);
            bridge.spawn(async move {
                if let Some(old) = restarted {
                    old.shutdown().await;
                }
                move |bridge: &mut Bridge| probe(bridge, slug, reply, attempts + 1)
            });
        }),
    );
}

/// Agent-supplied text for a terminal prompt: control characters (escape
/// sequences, carriage returns, newlines) are shown escaped, so they cannot
/// rewrite what the user is asked to confirm.
fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' | '\'' | '\\' => out.push(c),
            c => out.extend(c.escape_debug()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn printable_escapes_control_characters_only() {
        assert_eq!(
            printable("/tmp/evil:\u{1b}[2K\r  PATH=/usr/bin\n"),
            "/tmp/evil:\\u{1b}[2K\\r  PATH=/usr/bin\\n"
        );
        assert_eq!(printable(r#"C:\a "b" é"#), r#"C:\a "b" é"#);
    }
}
