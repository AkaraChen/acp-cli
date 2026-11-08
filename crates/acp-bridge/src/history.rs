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

/// Pages `--last` reads before picking the newest session: agents need not
/// list newest first.
const LAST_PAGES: usize = 10;

/// `thread/list` → `session/list` on the current agent.
pub(crate) fn list(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, params: ThreadListParams) {
    let agent = bridge.agents.current.clone();
    // `--last` asks for one thread from the start of the list.
    let last = params.limit == Some(1) && params.cursor.is_none();
    // session/list filters by one cwd; several (a repository's checkouts)
    // are filtered here. No filter lists everything.
    let (cwd, cwds) = match params.cwd {
        Some(ThreadListCwdFilter::One(cwd)) => (Some(cwd), None),
        Some(ThreadListCwdFilter::Many(mut cwds)) if cwds.len() == 1 => (cwds.pop(), None),
        Some(ThreadListCwdFilter::Many(cwds)) => (None, Some(cwds)),
        None => (None, None),
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
            let pages = if last { LAST_PAGES } else { 1 };
            bridge.spawn(async move {
                let result = list_pages(&connection, request, pages).await;
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
                            crate::threads::note_resumable(bridge, &thread_id, &connection);
                            if let Some(thread) = bridge.threads.get(&thread_id) {
                                let in_cwds = cwds.as_ref().is_none_or(|cwds| {
                                    let cwd = thread.cwd.as_path().display().to_string();
                                    cwds.contains(&cwd)
                                });
                                let matches = in_cwds
                                    && search.as_ref().is_none_or(|term| {
                                        thread.preview.to_lowercase().contains(term)
                                            || thread.title.as_deref().is_some_and(|title| {
                                                title.to_lowercase().contains(term)
                                            })
                                    });
                                if matches {
                                    let updated_at = parse_time(info.updated_at.as_deref());
                                    data.push((updated_at, records::thread(thread, false)));
                                }
                            }
                        }
                        // Most recent first, as the picker and `--last` expect;
                        // sessions the agent gave no time for go last.
                        // The agent sizes pages; `limit` is not applied, since
                        // dropping part of a page would skip it for good.
                        data.sort_by_key(|(updated_at, _)| std::cmp::Reverse(*updated_at));
                        let data = data.into_iter().map(|(_, thread)| thread).collect();
                        bridge.respond(
                            conn,
                            id,
                            ThreadListResponse {
                                data,
                                read_states: None,
                                next_cursor: if last { None } else { response.next_cursor },
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

/// Up to `pages` pages of `session/list`, as one response.
async fn list_pages(
    connection: &AgentConnection,
    request: acp::ListSessionsRequest,
    pages: usize,
) -> Result<acp::ListSessionsResponse, acp_client::AcpError> {
    let mut response = connection.list_sessions(request.clone()).await?;
    for _ in 1..pages {
        let Some(cursor) = response.next_cursor.take().filter(|cursor| !cursor.is_empty()) else {
            break;
        };
        let next = connection.list_sessions(request.clone().cursor(cursor)).await?;
        response.sessions.extend(next.sessions);
        response.next_cursor = next.next_cursor;
    }
    Ok(response)
}

type Located = Box<dyn FnOnce(&mut Bridge, Result<(), String>) + Send>;

/// Make sure `thread_id` is registered, searching the agents' session lists
/// for ids from an earlier run (ids are UUIDv5(agent, session id), so a
/// match also names the agent). The current agent is searched first.
fn locate(bridge: &mut Bridge, thread_id: String, then: Located) {
    if bridge.threads.get(&thread_id).is_some() {
        then(bridge, Ok(()));
        return;
    }
    let current = bridge.agents.current.clone();
    let mut slugs: Vec<String> = bridge
        .agents
        .profiles()
        .iter()
        .map(|profile| profile.slug.clone())
        .filter(|slug| slug != &current)
        .collect();
    slugs.insert(0, current);
    search(bridge, thread_id, slugs, None, then);
}

fn search(
    bridge: &mut Bridge,
    thread_id: String,
    mut slugs: Vec<String>,
    error: Option<String>,
    then: Located,
) {
    if slugs.is_empty() {
        then(
            bridge,
            Err(error.unwrap_or_else(|| format!("no agent has a session with id {thread_id}"))),
        );
        return;
    }
    let agent = slugs.remove(0);
    crate::agents::with_agent(
        bridge,
        &agent.clone(),
        Box::new(move |bridge, connection| {
            let connection = match connection {
                Ok(connection) if connection.caps().list_sessions() => connection,
                Ok(_) => return search(bridge, thread_id, slugs, error, then),
                Err(err) => return search(bridge, thread_id, slugs, error.or(Some(err)), then),
            };
            bridge.spawn(async move {
                let found = find_session(&connection, &agent, &thread_id).await;
                move |bridge: &mut Bridge| match found {
                    Ok(Some(info)) => {
                        let name = crate::threads::agent_display_name(bridge, &agent, &connection);
                        if let Some(thread_id) = register_listed(bridge, &agent, &name, &info) {
                            crate::threads::note_resumable(bridge, &thread_id, &connection);
                        }
                        then(bridge, Ok(()));
                    }
                    Ok(None) => search(bridge, thread_id, slugs, error, then),
                    Err(err) => {
                        let message = if err.is_auth_required() {
                            format!("agent `{agent}` requires sign-in; run `acpc login -a {agent}`")
                        } else {
                            format!("agent `{agent}` could not list sessions: {}", err.message)
                        };
                        search(bridge, thread_id, slugs, Some(message), then);
                    }
                }
            });
        }),
    );
}

/// `thread/read`.
pub(crate) fn read(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    thread_id: &str,
    include_turns: bool,
) {
    let key = thread_id.to_string();
    locate(
        bridge,
        thread_id.to_string(),
        Box::new(move |bridge, located| match (located, bridge.threads.get(&key)) {
            (Ok(()), Some(thread)) => {
                let response = ThreadReadResponse {
                    thread: records::thread(thread, include_turns),
                    read_state: None,
                };
                bridge.respond(conn, id, response);
            }
            (Err(err), _) => bridge.respond_error(conn, id, INVALID_REQUEST, err),
            (Ok(()), None) => {
                bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {key}"));
            }
        }),
    );
}

/// `thread/resume`: attach a listed session with `session/load` (history
/// replayed) or `session/resume` (no history), per agent capabilities.
pub(crate) fn resume(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, thread_id: String) {
    let key = thread_id.clone();
    locate(
        bridge,
        thread_id,
        Box::new(move |bridge, located| {
            if let Err(err) = located {
                bridge.respond_error(conn, id, INVALID_REQUEST, err);
                return;
            }
            let Some(thread) = bridge.threads.get(&key) else {
                bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {key}"));
                return;
            };
            if thread.replay.is_some() {
                bridge.respond_error(conn, id, INVALID_REQUEST, "this session is already being opened");
                return;
            }
            if thread.loaded {
                respond_loaded(bridge, conn, id, &key);
                return;
            }
            let agent = thread.agent.clone();
            attach(bridge, conn, id, agent, key);
        }),
    );
}

/// Answer a resume of a thread that is already open.
fn respond_loaded(bridge: &Bridge, conn: ConnectionId, id: RequestId, thread_id: &str) {
    let Some(thread) = bridge.threads.get(thread_id) else {
        return;
    };
    bridge.respond(conn, id, records::thread_resume_response(thread));
    crate::ext::thread_state(bridge, thread);
    // Approvals the client dropped while it was elsewhere.
    bridge.resend_server_requests(conn, thread_id);
}

async fn find_session(
    connection: &AgentConnection,
    agent: &str,
    thread_id: &str,
) -> Result<Option<acp::SessionInfo>, acp_client::AcpError> {
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let mut request = acp::ListSessionsRequest::new();
        if let Some(cursor) = cursor.take() {
            request = request.cursor(cursor);
        }
        let response = connection.list_sessions(request).await?;
        if let Some(info) = response
            .sessions
            .into_iter()
            .find(|info| thread_id_for(agent, &info.session_id.to_string()) == thread_id)
        {
            return Ok(Some(info));
        }
        cursor = response.next_cursor.filter(|cursor| !cursor.is_empty());
        if cursor.is_none() {
            break;
        }
    }
    Ok(None)
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
            // Checked again: another resume may have opened it while the
            // agent was starting.
            if thread.replay.is_some() {
                bridge.respond_error(conn, id, INVALID_REQUEST, "this session is already being opened");
                return;
            }
            if thread.loaded {
                respond_loaded(bridge, conn, id, &thread_id);
                return;
            }
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
            let Some(agent) = bridge.threads.get(&thread_id).map(|thread| thread.agent.clone()) else {
                return;
            };
            // The session died with its process; leave the thread unloaded.
            if result.is_ok() && !bridge.agents.is_live(&agent, &connection) {
                bridge.respond_error(conn, id, INTERNAL_ERROR, format!("agent `{agent}` exited; try again"));
                return;
            }
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
