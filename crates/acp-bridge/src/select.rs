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
use codex_app_server_protocol::AcpcNoticeNotification;
use codex_app_server_protocol::AcpcAuthMethod;
use codex_app_server_protocol::AcpcAuthSelectParams;
use codex_app_server_protocol::AcpcAuthSelectResponse;
use codex_app_server_protocol::AcpcTerminalRunParams;
use codex_app_server_protocol::AcpcTerminalRunResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_transport::ConnectionId;

use crate::server::Bridge;
use crate::server::INTERNAL_ERROR;
use crate::server::INVALID_PARAMS;

/// Sign-in attempts before giving up on an agent that keeps asking.
const MAX_AUTH_ATTEMPTS: u32 = 2;

/// Runs once a sign-in attempt ends; `Err` carries what to tell the user.
type SignedIn = Box<dyn FnOnce(&mut Bridge, Result<(), String>) + Send>;

#[derive(Clone)]
struct Reply {
    conn: ConnectionId,
    id: RequestId,
    /// Thread whose conversation the agent's next thread carries over.
    bring: Option<String>,
}

impl Reply {
    fn error(&self, bridge: &Bridge, message: impl Into<String>) {
        bridge.respond_error(self.conn, self.id.clone(), INTERNAL_ERROR, message);
    }
}

pub(crate) fn select(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    slug: String,
    bring: Option<String>,
) {
    if bridge.agents.profile(&slug).is_none() {
        bridge.respond_error(conn, id, INVALID_PARAMS, format!("unknown agent `{slug}`"));
        return;
    }
    probe(bridge, slug, Reply { conn, id, bring }, 0);
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
                        bridge.agents.handoff = reply.bring.clone().map(|from| (slug.clone(), from));
                        bridge.agents.current = slug;
                        bridge.respond(reply.conn, reply.id.clone(), AcpcAgentSelectResponse {});
                        crate::ext::agents_updated(bridge);
                    }
                    Err(err) if err.is_auth_required() && attempts < MAX_AUTH_ATTEMPTS => {
                        let retry = slug.clone();
                        let done: SignedIn = Box::new(move |bridge, result| match result {
                            Ok(()) => probe(bridge, retry, reply, attempts + 1),
                            Err(err) => reply.error(bridge, err),
                        });
                        sign_in(bridge, slug, connection, done);
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

/// A prompt failed with `auth_required` (agents may check credentials only
/// when they call their model): offer the same sign-in, then tell the user
/// to send again.
pub(crate) fn sign_in_after_prompt(bridge: &mut Bridge, thread_id: &str) {
    let Some(slug) = bridge.threads.get(thread_id).map(|thread| thread.agent.clone()) else {
        return;
    };
    let Some(connection) = bridge.agents.connection(&slug) else {
        return;
    };
    let thread_id = thread_id.to_string();
    let done: SignedIn = Box::new(move |bridge, result| {
        // Terminal sign-in restarts the agent, which unloads the thread.
        let loaded = bridge.threads.get(&thread_id).is_some_and(|thread| thread.loaded);
        let (severity, title) = match result {
            Ok(()) if loaded => ("info", "Signed in. Send your message again.".to_string()),
            Ok(()) => (
                "info",
                "Signed in. Run /resume to reopen this session, then send your message again."
                    .to_string(),
            ),
            Err(err) => ("warning", format!("Not signed in: {err}")),
        };
        bridge.notify(ServerNotification::AcpcNotice(AcpcNoticeNotification {
            thread_id: Some(thread_id),
            severity: severity.to_string(),
            title,
            description: None,
        }));
    });
    sign_in(bridge, slug, connection, done);
}

/// Ask the user for a sign-in method and run it.
fn sign_in(bridge: &mut Bridge, slug: String, connection: Arc<AgentConnection>, done: SignedIn) {
    let methods = auth_methods(&connection);
    let agent_name = bridge
        .agents
        .profile(&slug)
        .map_or_else(|| slug.clone(), |profile| profile.name.clone());
    if methods.is_empty() {
        done(
            bridge,
            Err(format!(
                "{agent_name} requires sign-in but offers no ACP sign-in method; sign in with its own tools"
            )),
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
                done(bridge, Err("sign-in cancelled".to_string()));
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
                        move |bridge: &mut Bridge| {
                            done(bridge, result.map_err(|err| format!("sign-in failed: {}", err.message)));
                        }
                    });
                }
                Some(acp::AuthMethod::Terminal(terminal)) => {
                    terminal_sign_in(bridge, slug, &terminal, done);
                }
                _ => done(bridge, Err(format!("unknown sign-in method `{method_id}`"))),
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
fn terminal_sign_in(bridge: &mut Bridge, slug: String, method: &acp::AuthMethodTerminal, done: SignedIn) {
    if !bridge.options.client.terminal_auth {
        done(bridge, Err("terminal sign-in is not available in this client".to_string()));
        return;
    }
    let Some(profile) = bridge.agents.profile(&slug).cloned() else {
        done(bridge, Err(format!("unknown agent `{slug}`")));
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
                done(bridge, Err("sign-in did not complete".to_string()));
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
                move |bridge: &mut Bridge| done(bridge, Ok(()))
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
