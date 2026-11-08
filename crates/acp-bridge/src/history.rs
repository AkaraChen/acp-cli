//! Session listing, loading and history reconstruction.
//!
//! `session/load` replays the conversation as ordinary `session/update`
//! notifications before its response (session-setup.mdx). The replay has no
//! turn boundaries, so a new turn starts at a user message that follows
//! agent output. The rebuilt turns are returned in `thread/resume`.

use std::sync::Arc;

use acp_client::AgentConnection;
use acp_client::LoadId;
use acp_client::schema as acp;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadListCwdFilter;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_transport::ConnectionId;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::records;
use crate::server::Bridge;
use crate::server::INTERNAL_ERROR;
use crate::server::INVALID_REQUEST;
use crate::threads::Replay;
use crate::threads::ThreadState;
use crate::threads::thread_id_for;
use crate::turn::TurnBuilder;

fn parse_time(value: Option<&str>) -> Option<i64> {
    let value = value?;
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp())
}

/// Register (or refresh) a thread for a session the agent listed.
fn register_listed(
    bridge: &mut Bridge,
    agent: &str,
    agent_name: &str,
    info: &acp::SessionInfo,
) -> Option<String> {
    let session_id = info.session_id.to_string();
    let id = thread_id_for(agent, &session_id);
    let updated_at = parse_time(info.updated_at.as_deref());
    if let Some(thread) = bridge.threads.get_mut(&id) {
        if let Some(title) = &info.title {
            thread.title = Some(title.clone());
        }
        if let Some(updated_at) = updated_at {
            thread.updated_at = updated_at;
        }
        return Some(id);
    }
    // session/list cwds are absolute (overview.mdx); skip anything else.
    let cwd = records::absolute(&info.cwd)?;
    let mut state = ThreadState::new(id.clone(), agent.to_string(), agent_name.to_string(), cwd);
    state.session_id = Some(info.session_id.clone());
    state.title = info.title.clone();
    state.preview = info.title.clone().unwrap_or_default();
    if let Some(updated_at) = updated_at {
        state.updated_at = updated_at;
        state.created_at = updated_at;
    }
    bridge.threads.insert(state);
    Some(id)
}

/// `thread/list` → `session/list` on the current agent.
pub(crate) fn list(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, params: ThreadListParams) {
    let agent = bridge.agents.current.clone();
    let cwd = match params.cwd {
        Some(ThreadListCwdFilter::One(cwd)) => Some(cwd),
        Some(ThreadListCwdFilter::Many(mut cwds)) if cwds.len() == 1 => cwds.pop(),
        _ => None,
    };
    let cursor = params.cursor;
    let search = params.search_term.map(|term| term.to_lowercase());
    crate::agents::with_agent(
        bridge,
        &agent.clone(),
        Box::new(move |bridge, connection| {
            let connection = match connection {
                Ok(connection) => connection,
                Err(err) => {
                    bridge.respond_error(conn, id, INTERNAL_ERROR, err);
                    return;
                }
            };
            if !connection.caps().list_sessions() {
                bridge.respond(
                    conn,
                    id,
                    ThreadListResponse {
                        data: Vec::new(),
                        read_states: None,
                        next_cursor: None,
                        backwards_cursor: None,
                    },
                );
                return;
            }
            let mut request = acp::ListSessionsRequest::new();
            if let Some(cwd) = cwd {
                request = request.cwd(std::path::PathBuf::from(cwd));
            }
            if let Some(cursor) = cursor {
                request = request.cursor(cursor);
            }
            bridge.spawn(async move {
                let result = connection.list_sessions(request).await;
                Box::new(move |bridge: &mut Bridge| match result {
                    Ok(response) => {
                        let agent_name =
                            crate::threads::agent_display_name(bridge, &agent, &connection);
                        let mut data = Vec::new();
                        for info in &response.sessions {
                            let Some(thread_id) = register_listed(bridge, &agent, &agent_name, info)
                            else {
                                continue;
                            };
                            if let Some(thread) = bridge.threads.get(&thread_id) {
                                let matches = search.as_ref().is_none_or(|term| {
                                    thread.preview.to_lowercase().contains(term)
                                        || thread
                                            .title
                                            .as_deref()
                                            .is_some_and(|title| title.to_lowercase().contains(term))
                                });
                                if matches {
                                    data.push(records::thread(thread, false));
                                }
                            }
                        }
                        bridge.respond(
                            conn,
                            id,
                            ThreadListResponse {
                                data,
                                read_states: None,
                                next_cursor: response.next_cursor,
                                backwards_cursor: None,
                            },
                        );
                    }
                    Err(err) => bridge.respond_error(conn, id, INTERNAL_ERROR, err.message),
                })
            });
        }),
    );
}

/// `thread/read`.
pub(crate) fn read(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, thread_id: &str, include_turns: bool) {
    match bridge.threads.get(thread_id) {
        Some(thread) => {
            let response = ThreadReadResponse {
                thread: records::thread(thread, include_turns),
                read_state: None,
            };
            bridge.respond(conn, id, response);
        }
        None => bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {thread_id}")),
    }
}

/// `thread/resume`: attach a listed session with `session/load` (history
/// replayed) or `session/resume` (no history), per agent capabilities.
pub(crate) fn resume(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, thread_id: String) {
    if let Some(thread) = bridge.threads.get(&thread_id) {
        if thread.loaded {
            let response = records::thread_resume_response(thread);
            bridge.respond(conn, id, response);
            if let Some(thread) = bridge.threads.get(&thread_id) {
                crate::ext::thread_state(bridge, thread);
            }
            return;
        }
        let agent = thread.agent.clone();
        attach(bridge, conn, id, agent, thread_id);
        return;
    }
    // Unknown id (e.g. from an earlier run): find it among the current
    // agent's sessions; ids are derived from (agent, session id).
    let agent = bridge.agents.current.clone();
    crate::agents::with_agent(
        bridge,
        &agent.clone(),
        Box::new(move |bridge, connection| {
            let connection = match connection {
                Ok(connection) if connection.caps().list_sessions() => connection,
                Ok(_) => {
                    bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {thread_id}"));
                    return;
                }
                Err(err) => {
                    bridge.respond_error(conn, id, INTERNAL_ERROR, err);
                    return;
                }
            };
            bridge.spawn(async move {
                let found = find_session(&connection, &agent, &thread_id).await;
                Box::new(move |bridge: &mut Bridge| match found {
                    Some(info) => {
                        let name = crate::threads::agent_display_name(bridge, &agent, &connection);
                        register_listed(bridge, &agent, &name, &info);
                        attach(bridge, conn, id, agent, thread_id);
                    }
                    None => bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {thread_id}")),
                })
            });
        }),
    );
}

async fn find_session(connection: &AgentConnection, agent: &str, thread_id: &str) -> Option<acp::SessionInfo> {
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let mut request = acp::ListSessionsRequest::new();
        if let Some(cursor) = cursor.take() {
            request = request.cursor(cursor);
        }
        let response = connection.list_sessions(request).await.ok()?;
        if let Some(info) = response
            .sessions
            .into_iter()
            .find(|info| thread_id_for(agent, &info.session_id.to_string()) == thread_id)
        {
            return Some(info);
        }
        cursor = response.next_cursor.filter(|cursor| !cursor.is_empty());
        cursor.as_ref()?;
    }
    None
}

fn attach(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, agent: String, thread_id: String) {
    let mcp_servers = bridge.options.mcp_servers.clone();
    crate::agents::with_agent(
        bridge,
        &agent.clone(),
        Box::new(move |bridge, connection| {
            let connection = match connection {
                Ok(connection) => connection,
                Err(err) => {
                    bridge.respond_error(conn, id, INTERNAL_ERROR, err);
                    return;
                }
            };
            let Some(thread) = bridge.threads.get_mut(&thread_id) else {
                bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {thread_id}"));
                return;
            };
            let Some(session_id) = thread.session_id.clone() else {
                bridge.respond_error(conn, id, INVALID_REQUEST, "thread has no session");
                return;
            };
            let cwd = thread.cwd.as_path().to_path_buf();
            let mcp = connection.caps().filter_mcp_servers(mcp_servers);
            thread.image_input = connection.caps().prompt_image();
            if connection.caps().load_session() {
                thread.turns.clear();
                thread.replay = Some(Replay {
                    load_id: None,
                    turns: Vec::new(),
                    current: None,
                    reply: Some((conn, id.clone())),
                });
                let request = acp::LoadSessionRequest::new(session_id, cwd).mcp_servers(mcp);
                match connection.load_session(request) {
                    Ok(load_id) => {
                        if let Some(replay) = thread.replay.as_mut() {
                            replay.load_id = Some(load_id);
                        }
                    }
                    Err(err) => {
                        thread.replay = None;
                        bridge.respond_error(conn, id, INTERNAL_ERROR, err.message);
                    }
                }
            } else if connection.caps().resume_session() {
                let request = acp::ResumeSessionRequest::new(session_id, cwd).mcp_servers(mcp);
                resume_without_history(bridge, conn, id, thread_id, connection, request);
            } else {
                bridge.respond_error(
                    conn,
                    id,
                    INVALID_REQUEST,
                    format!("agent `{agent}` cannot reopen sessions"),
                );
            }
        }),
    );
}

fn resume_without_history(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    thread_id: String,
    connection: Arc<AgentConnection>,
    request: acp::ResumeSessionRequest,
) {
    bridge.spawn(async move {
        let result = connection.resume_session(request).await;
        Box::new(move |bridge: &mut Bridge| {
            let Some(thread) = bridge.threads.get_mut(&thread_id) else {
                return;
            };
            match result {
                Ok(response) => {
                    thread.loaded = true;
                    thread.apply_session_setup(response.config_options, response.modes);
                    let reply = records::thread_resume_response(thread);
                    bridge.respond(conn, id, reply);
                    if let Some(thread) = bridge.threads.get(&thread_id) {
                        crate::ext::thread_state(bridge, thread);
                    }
                }
                Err(err) => bridge.respond_error(conn, id, INTERNAL_ERROR, err.message),
            }
        })
    });
}

/// Fold one replayed update into the history being rebuilt.
pub(crate) fn on_replay_update(thread: &mut ThreadState, update: acp::SessionUpdate) {
    let thread_id = thread.id.clone();
    let cwd = thread.cwd.as_path().to_path_buf();
    let Some(replay) = thread.replay.as_mut() else {
        return;
    };
    let mut discard = Vec::new();
    let starts_turn = matches!(update, acp::SessionUpdate::UserMessageChunk(_))
        && replay
            .current
            .as_ref()
            .is_some_and(TurnBuilder::has_agent_output);
    if starts_turn && let Some(builder) = replay.current.take() {
        replay
            .turns
            .push(builder.finish(TurnStatus::Completed, None, &mut discard));
    }
    let builder = replay.current.get_or_insert_with(|| {
        let index = replay.turns.len() + 1;
        TurnBuilder::new(format!("{thread_id}-r{index}"), thread_id.clone(), cwd, records::now_ms())
    });
    match update {
        acp::SessionUpdate::UserMessageChunk(chunk) => builder.user_chunk(&chunk, &mut discard),
        acp::SessionUpdate::AgentMessageChunk(chunk) => builder.agent_message(&chunk, &mut discard),
        acp::SessionUpdate::AgentThoughtChunk(chunk) => builder.agent_thought(&chunk, &mut discard),
        acp::SessionUpdate::ToolCall(call) => builder.tool_call(call, &mut discard),
        acp::SessionUpdate::ToolCallUpdate(update) => builder.tool_call_update(update, &mut discard),
        acp::SessionUpdate::CompactionUpdate(update) => builder.compaction(&update, &mut discard),
        _ => {}
    }
}

pub(crate) fn on_load_finished(
    bridge: &mut Bridge,
    agent: &str,
    session_id: &acp::SessionId,
    load_id: LoadId,
    result: Result<acp::LoadSessionResponse, acp_client::AcpError>,
) {
    let Some(thread) = bridge.threads.by_session_mut(agent, session_id) else {
        return;
    };
    let matches = thread
        .replay
        .as_ref()
        .is_some_and(|replay| replay.load_id.is_none_or(|id| id == load_id));
    if !matches {
        return;
    }
    let Some(mut replay) = thread.replay.take() else {
        return;
    };
    let mut discard = Vec::new();
    if let Some(builder) = replay.current.take()
        && (builder.has_items())
    {
        replay
            .turns
            .push(builder.finish(TurnStatus::Completed, None, &mut discard));
    }
    let Some((conn, id)) = replay.reply.take() else {
        return;
    };
    match result {
        Ok(response) => {
            thread.turns = replay.turns;
            thread.loaded = true;
            if thread.preview.is_empty() {
                thread.preview = first_user_text(thread).unwrap_or_default();
            }
            thread.apply_session_setup(response.config_options, response.modes);
            let reply = records::thread_resume_response(thread);
            let thread_id = thread.id.clone();
            bridge.respond(conn, id, reply);
            if let Some(thread) = bridge.threads.get(&thread_id) {
                crate::ext::thread_state(bridge, thread);
            }
        }
        Err(err) => bridge.respond_error(conn, id, INTERNAL_ERROR, err.message),
    }
}

fn first_user_text(thread: &ThreadState) -> Option<String> {
    thread.turns.iter().flat_map(|turn| turn.items.iter()).find_map(|item| match &item.payload {
        codex_app_server_protocol::ThreadItemPayload::UserMessage { content, .. } => {
            content.iter().find_map(|input| match input {
                codex_app_server_protocol::UserInput::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
        }
        _ => None,
    })
}

pub(crate) fn absolute_cwd(bridge: &Bridge, cwd: Option<&str>) -> AbsolutePathBuf {
    cwd.and_then(|cwd| records::absolute(std::path::Path::new(cwd)))
        .unwrap_or_else(|| bridge.options.default_cwd.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn text(t: &str) -> acp::ContentChunk {
        acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(t)))
    }

    #[test]
    fn replay_splits_turns_at_user_messages_after_agent_output() {
        let mut thread = ThreadState::new(
            "t".into(),
            "a".into(),
            "A".into(),
            records::absolute(std::path::Path::new("/w")).expect("absolute"),
        );
        thread.replay = Some(Replay {
            load_id: None,
            turns: Vec::new(),
            current: None,
            reply: None,
        });
        for update in [
            acp::SessionUpdate::UserMessageChunk(text("hi ")),
            acp::SessionUpdate::UserMessageChunk(text("there")),
            acp::SessionUpdate::AgentMessageChunk(text("hello")),
            acp::SessionUpdate::UserMessageChunk(text("again")),
            acp::SessionUpdate::AgentMessageChunk(text("ok")),
        ] {
            on_replay_update(&mut thread, update);
        }
        let mut replay = thread.replay.take().expect("replay");
        let last = replay.current.take().expect("open turn");
        replay.turns.push(last.finish(TurnStatus::Completed, None, &mut Vec::new()));
        assert_eq!(replay.turns.len(), 2);
        assert_eq!(replay.turns[0].items.len(), 2);
        assert!(matches!(
            &replay.turns[0].items[0].payload,
            codex_app_server_protocol::ThreadItemPayload::UserMessage { content, .. }
                if matches!(&content[0], codex_app_server_protocol::UserInput::Text { text, .. } if text == "hi there")
        ));
    }
}
