//! Threads (Codex) ↔ sessions (ACP): lifecycle, turns, and event routing.

use std::collections::HashMap;
use std::sync::Arc;

use acp_client::AgentConnection;
use acp_client::AgentEvent;
use acp_client::LoadId;
use acp_client::PromptId;
use acp_client::schema as acp;
use codex_app_server_protocol::ErrorNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadNameUpdatedNotification;
use codex_app_server_protocol::ThreadSettingsUpdatedNotification;
use codex_app_server_protocol::ThreadStartedNotification;
use codex_app_server_protocol::ThreadTokenUsage;
use codex_app_server_protocol::ThreadTokenUsageUpdatedNotification;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_app_server_protocol::WarningNotification;
use codex_app_server_transport::ConnectionId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_utils_absolute_path::AbsolutePathBuf;
use uuid::Uuid;

use crate::models;
use crate::records;
use crate::server::Bridge;
use crate::turn::Emit;
use crate::turn::TurnBuilder;

/// Namespace for deriving stable thread ids from (agent, session id).
const THREAD_NAMESPACE: Uuid = Uuid::from_u128(0x6163_7063_2d74_6872_6561_642d_6964_7331);

pub(crate) fn thread_id_for(agent: &str, session_id: &str) -> String {
    Uuid::new_v5(&THREAD_NAMESPACE, format!("{agent}\n{session_id}").as_bytes()).to_string()
}

pub(crate) struct ActiveTurn {
    pub(crate) builder: TurnBuilder,
    pub(crate) prompt_id: Option<PromptId>,
    pub(crate) cancel_requested: bool,
}

/// History reconstruction while `session/load` replays the conversation.
pub(crate) struct Replay {
    pub(crate) load_id: Option<LoadId>,
    pub(crate) turns: Vec<Turn>,
    pub(crate) current: Option<TurnBuilder>,
    pub(crate) reply: Option<(ConnectionId, RequestId)>,
}

pub(crate) struct ThreadState {
    pub(crate) id: String,
    pub(crate) agent: String,
    pub(crate) session_id: Option<acp::SessionId>,
    /// The session is attached on its agent connection (new/load/resume).
    pub(crate) loaded: bool,
    pub(crate) cwd: AbsolutePathBuf,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
    pub(crate) title: Option<String>,
    pub(crate) preview: String,
    pub(crate) config_options: Vec<acp::SessionConfigOption>,
    pub(crate) modes: Option<acp::SessionModeState>,
    pub(crate) commands: Vec<acp::AvailableCommand>,
    pub(crate) turns: Vec<Turn>,
    pub(crate) active: Option<ActiveTurn>,
    pub(crate) replay: Option<Replay>,
    pub(crate) usage: Option<(u64, u64)>,
    pub(crate) image_input: bool,
    pub(crate) agent_name: String,
}

impl ThreadState {
    pub(crate) fn new(id: String, agent: String, agent_name: String, cwd: AbsolutePathBuf) -> Self {
        let now = records::now_secs();
        Self {
            id,
            agent,
            session_id: None,
            loaded: false,
            cwd,
            created_at: now,
            updated_at: now,
            title: None,
            preview: String::new(),
            config_options: Vec::new(),
            modes: None,
            commands: Vec::new(),
            turns: Vec::new(),
            active: None,
            replay: None,
            usage: None,
            image_input: false,
            agent_name,
        }
    }

    pub(crate) fn model_label(&self) -> String {
        models::current_model(&self.config_options)
    }

    pub(crate) fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        models::current_effort(&self.config_options)
    }

    pub(crate) fn apply_session_setup(
        &mut self,
        config_options: Option<Vec<acp::SessionConfigOption>>,
        modes: Option<acp::SessionModeState>,
    ) {
        if let Some(options) = config_options {
            self.config_options = options;
        }
        if modes.is_some() {
            self.modes = modes;
        }
    }
}

/// Updates kept per not-yet-registered session (agents may notify right
/// after `session/new` responds, before the bridge registers the thread).
const MAX_ORPHAN_UPDATES: usize = 256;

#[derive(Default)]
pub(crate) struct Threads {
    by_id: HashMap<String, ThreadState>,
    orphans: HashMap<String, Vec<acp::SessionNotification>>,
}

impl Threads {
    pub(crate) fn get(&self, id: &str) -> Option<&ThreadState> {
        self.by_id.get(id)
    }

    pub(crate) fn get_mut(&mut self, id: &str) -> Option<&mut ThreadState> {
        self.by_id.get_mut(id)
    }

    pub(crate) fn insert(&mut self, state: ThreadState) {
        self.by_id.insert(state.id.clone(), state);
    }

    pub(crate) fn remove(&mut self, id: &str) -> Option<ThreadState> {
        self.by_id.remove(id)
    }

    pub(crate) fn loaded_ids(&self) -> Vec<String> {
        self.by_id
            .values()
            .filter(|thread| thread.loaded)
            .map(|thread| thread.id.clone())
            .collect()
    }

    pub(crate) fn by_session_mut(&mut self, agent: &str, session_id: &acp::SessionId) -> Option<&mut ThreadState> {
        let id = thread_id_for(agent, &session_id.to_string());
        self.by_id.get_mut(&id)
    }

    fn keep_orphan(&mut self, agent: &str, notification: acp::SessionNotification) {
        let key = thread_id_for(agent, &notification.session_id.to_string());
        let updates = self.orphans.entry(key).or_default();
        if updates.len() < MAX_ORPHAN_UPDATES {
            updates.push(notification);
        }
    }

    pub(crate) fn take_orphans(&mut self, thread_id: &str) -> Vec<acp::SessionNotification> {
        self.orphans.remove(thread_id).unwrap_or_default()
    }

    /// The loaded thread most recently active, on any agent.
    pub(crate) fn most_recent_loaded(&self) -> Option<String> {
        self.by_id
            .values()
            .filter(|thread| thread.loaded)
            .max_by_key(|thread| thread.updated_at)
            .map(|thread| thread.id.clone())
    }

    pub(crate) fn on_agent(&self, agent: &str) -> Vec<String> {
        self.by_id
            .values()
            .filter(|thread| thread.agent == agent)
            .map(|thread| thread.id.clone())
            .collect()
    }
}

pub(crate) fn emit_all(bridge: &Bridge, emit: Emit) {
    for notification in emit {
        bridge.notify(notification);
    }
}

/// Route an agent event to the thread that owns its session.
pub(crate) fn on_agent_event(bridge: &mut Bridge, agent: &str, event: AgentEvent) {
    match event {
        AgentEvent::SessionUpdate(notification) => {
            on_session_update(bridge, agent, notification);
        }
        AgentEvent::PermissionRequested { id, request } => {
            crate::approvals::on_permission(bridge, agent, id, request);
        }
        AgentEvent::ElicitationRequested { id, request } => {
            crate::approvals::on_elicitation(bridge, agent, id, request);
        }
        AgentEvent::ElicitationCompleted(notification) => {
            crate::approvals::on_elicitation_complete(bridge, agent, &notification);
        }
        AgentEvent::PendingRequestResolved { id } => {
            crate::approvals::on_pending_resolved(bridge, agent, id);
        }
        AgentEvent::PromptFinished {
            session_id,
            prompt_id,
            result,
        } => on_prompt_finished(bridge, agent, &session_id, prompt_id, result),
        AgentEvent::LoadFinished {
            session_id,
            load_id,
            result,
        } => crate::history::on_load_finished(bridge, agent, &session_id, load_id, result),
        AgentEvent::Exited { .. } => {}
    }
}

fn on_session_update(bridge: &mut Bridge, agent: &str, notification: acp::SessionNotification) {
    let Some(thread) = bridge
        .threads
        .by_session_mut(agent, &notification.session_id)
    else {
        tracing::debug!(%agent, session = %notification.session_id, "update for unregistered session");
        bridge.threads.keep_orphan(agent, notification);
        return;
    };
    let thread_id = thread.id.clone();
    let mut emit = Vec::new();
    // Session-scoped state first; these never belong to a turn.
    match &notification.update {
        acp::SessionUpdate::AvailableCommandsUpdate(update) => {
            thread.commands = update.available_commands.clone();
            crate::ext::commands_updated(thread, &mut emit);
            emit_all(bridge, emit);
            return;
        }
        acp::SessionUpdate::ConfigOptionUpdate(update) => {
            thread.config_options = update.config_options.clone();
            emit.push(settings_updated(thread));
            crate::ext::session_config_updated(thread, &mut emit);
            emit_all(bridge, emit);
            return;
        }
        acp::SessionUpdate::CurrentModeUpdate(update) => {
            if let Some(modes) = thread.modes.as_mut() {
                modes.current_mode_id = update.current_mode_id.clone();
            }
            crate::ext::session_config_updated(thread, &mut emit);
            emit_all(bridge, emit);
            return;
        }
        acp::SessionUpdate::SessionInfoUpdate(update) => {
            match &update.title {
                acp_client::MaybeUndefined::Value(title) => thread.title = Some(title.clone()),
                acp_client::MaybeUndefined::Null => thread.title = None,
                acp_client::MaybeUndefined::Undefined => return,
            }
            emit.push(ServerNotification::ThreadNameUpdated(
                ThreadNameUpdatedNotification {
                    thread_id,
                    thread_name: thread.title.clone(),
                },
            ));
            emit_all(bridge, emit);
            return;
        }
        acp::SessionUpdate::UsageUpdate(update) => {
            thread.usage = Some((update.used, update.size));
            let turn_id = thread
                .active
                .as_ref()
                .map(|active| active.builder.id.clone())
                .or_else(|| thread.turns.last().map(|turn| turn.id.clone()))
                .unwrap_or_default();
            if thread.replay.is_none() {
                emit.push(usage_updated(&thread_id, &turn_id, update.used, update.size));
            }
            emit_all(bridge, emit);
            return;
        }
        acp::SessionUpdate::Notice(notice) => {
            if thread.replay.is_none() {
                crate::ext::notice(&thread_id, notice, &mut emit);
            }
            emit_all(bridge, emit);
            return;
        }
        _ => {}
    }
    if thread.replay.is_some() {
        crate::history::on_replay_update(thread, notification.update);
        return;
    }
    let Some(active) = thread.active.as_mut() else {
        tracing::debug!(thread = %thread_id, "content update outside a turn ignored");
        return;
    };
    let builder = &mut active.builder;
    match notification.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => builder.agent_message(&chunk, &mut emit),
        acp::SessionUpdate::AgentThoughtChunk(chunk) => builder.agent_thought(&chunk, &mut emit),
        // The client already shows what the user typed.
        acp::SessionUpdate::UserMessageChunk(_) => {}
        acp::SessionUpdate::ToolCall(call) => builder.tool_call(call, &mut emit),
        acp::SessionUpdate::ToolCallUpdate(update) => builder.tool_call_update(update, &mut emit),
        acp::SessionUpdate::Plan(plan) => builder.plan(&plan, &mut emit),
        acp::SessionUpdate::CompactionUpdate(update) => builder.compaction(&update, &mut emit),
        // Summary text is not rendered by the Codex compaction cell.
        acp::SessionUpdate::CompactionSummaryChunk(_) => {}
        _ => {}
    }
    emit_all(bridge, emit);
}

pub(crate) fn settings_updated(thread: &ThreadState) -> ServerNotification {
    ServerNotification::ThreadSettingsUpdated(ThreadSettingsUpdatedNotification {
        thread_id: thread.id.clone(),
        thread_settings: records::thread_settings(thread),
    })
}

pub(crate) fn usage_updated(thread_id: &str, turn_id: &str, used: u64, size: u64) -> ServerNotification {
    let used = i64::try_from(used).unwrap_or(i64::MAX);
    let breakdown = TokenUsageBreakdown {
        total_tokens: used,
        input_tokens: used,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
    };
    ServerNotification::ThreadTokenUsageUpdated(ThreadTokenUsageUpdatedNotification {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        token_usage: ThreadTokenUsage {
            total: breakdown.clone(),
            last: breakdown,
            model_context_window: Some(i64::try_from(size).unwrap_or(i64::MAX)),
        },
    })
}

/// `thread/start`: connect the agent if needed, then `session/new`.
pub(crate) fn start(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    agent: String,
    cwd: AbsolutePathBuf,
) {
    let mcp_servers = bridge.options.mcp_servers.clone();
    let prepared = bridge.agents.take_prepared(&agent, &cwd);
    crate::agents::with_agent(
        bridge,
        &agent.clone(),
        Box::new(move |bridge, connection| {
            let connection = match connection {
                Ok(connection) => connection,
                Err(err) => {
                    bridge.respond_error(
                        conn,
                        id,
                        crate::server::INTERNAL_ERROR,
                        format!("failed to start agent `{agent}`: {err}"),
                    );
                    return;
                }
            };
            if let Some(response) = prepared {
                adopt_new_session(bridge, conn, id, &agent, cwd, &connection, response);
                return;
            }
            let request = acp::NewSessionRequest::new(cwd.as_path().to_path_buf())
                .mcp_servers(connection.caps().filter_mcp_servers(mcp_servers));
            bridge.spawn(async move {
                let result = connection.new_session(request).await;
                move |bridge: &mut Bridge| match result {
                    Ok(response) => {
                        adopt_new_session(bridge, conn, id, &agent, cwd, &connection, response);
                    }
                    Err(err) => {
                        let code = if err.is_auth_required() {
                            crate::server::INVALID_REQUEST
                        } else {
                            crate::server::INTERNAL_ERROR
                        };
                        bridge.respond_error(
                            conn,
                            id,
                            code,
                            format!("agent `{agent}` could not create a session: {}", err.message),
                        );
                    }
                }
            });
        }),
    );
}

fn adopt_new_session(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    agent: &str,
    cwd: AbsolutePathBuf,
    connection: &Arc<AgentConnection>,
    response: acp::NewSessionResponse,
) {
    // The session died with its process if the agent exited meanwhile.
    if !bridge.agents.is_live(agent, connection) {
        bridge.respond_error(
            conn,
            id,
            crate::server::INTERNAL_ERROR,
            format!("agent `{agent}` exited; try again"),
        );
        return;
    }
    let session_id = response.session_id.to_string();
    let thread_id = thread_id_for(agent, &session_id);
    let mut state = ThreadState::new(
        thread_id.clone(),
        agent.to_string(),
        agent_display_name(bridge, agent, connection),
        cwd,
    );
    state.session_id = Some(response.session_id);
    state.loaded = true;
    state.image_input = connection.caps().prompt_image();
    state.apply_session_setup(response.config_options, response.modes);
    let reply = records::thread_start_response(&state);
    let thread = reply.thread.clone();
    bridge.threads.insert(state);
    note_resumable(bridge, &thread_id, connection);
    for update in bridge.threads.take_orphans(&thread_id) {
        on_session_update(bridge, agent, update);
    }
    bridge.respond(conn, id, reply);
    bridge.notify(ServerNotification::ThreadStarted(ThreadStartedNotification { thread }));
    if let Some(state) = bridge.threads.get(&thread_id) {
        crate::ext::thread_state(bridge, state);
    }
}

/// Remember `thread_id` for the `acpc resume` hint if its agent can list
/// and reopen sessions.
pub(crate) fn note_resumable(bridge: &Bridge, thread_id: &str, connection: &AgentConnection) {
    let caps = connection.caps();
    if caps.list_sessions() && (caps.load_session() || caps.resume_session()) {
        bridge
            .options
            .resumable
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(thread_id.to_string());
    }
}

pub(crate) fn agent_display_name(bridge: &Bridge, agent: &str, connection: &AgentConnection) -> String {
    bridge
        .agents
        .profile(agent)
        .map(|profile| profile.name.clone())
        .or_else(|| connection.caps().agent_name().map(str::to_string))
        .unwrap_or_else(|| agent.to_string())
}

/// `turn/start`: forward the user's input as `session/prompt`.
pub(crate) fn start_turn(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    thread_id: &str,
    input: Vec<UserInput>,
    settings: TurnSettings,
) {
    let Some(thread) = bridge.threads.get(thread_id) else {
        bridge.respond_error(conn, id, crate::server::INVALID_REQUEST, format!("unknown thread {thread_id}"));
        return;
    };
    if thread.active.is_some() {
        bridge.respond_error(conn, id, crate::server::INVALID_REQUEST, "a turn is already running");
        return;
    }
    let Some(session_id) = thread.session_id.clone().filter(|_| thread.loaded) else {
        let message = if thread.session_id.is_some() {
            "The agent restarted since this session was opened; use /resume to reopen it."
        } else {
            "thread is not loaded"
        };
        bridge.respond_error(conn, id, crate::server::INVALID_REQUEST, message);
        return;
    };
    let agent = thread.agent.clone();
    let Some(connection) = bridge.agents.connection(&agent) else {
        bridge.respond_error(conn, id, crate::server::INTERNAL_ERROR, format!("agent `{agent}` is not running"));
        return;
    };
    let cwd = thread.cwd.as_path().to_path_buf();
    let prompt = crate::content::prompt_blocks(&input, connection.caps(), &cwd);
    if prompt.is_empty() {
        bridge.respond_error(conn, id, crate::server::INVALID_PARAMS, "empty prompt");
        return;
    }
    let current_options = thread.config_options.clone();

    let turn_id = Uuid::new_v4().to_string();
    let started_at = records::now_ms();
    let mut builder = TurnBuilder::new(turn_id.clone(), thread_id.to_string(), cwd, started_at);
    // Kept for history (`thread/read`); not emitted live, see `UserMessageChunk`.
    for block in &prompt {
        builder.user_chunk(&acp::ContentChunk::new(block.clone()), &mut Vec::new());
    }
    let thread_id_owned = thread_id.to_string();
    let Some(thread) = bridge.threads.get_mut(thread_id) else {
        return;
    };
    if thread.preview.is_empty() {
        thread.preview = input
            .iter()
            .find_map(|item| match item {
                UserInput::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
    }
    thread.updated_at = records::now_secs();
    thread.active = Some(ActiveTurn {
        builder,
        prompt_id: None,
        cancel_requested: false,
    });
    let turn = records::turn(&turn_id, Vec::new(), TurnStatus::InProgress, None, Some(started_at), None);
    bridge.respond(
        conn,
        id,
        codex_app_server_protocol::TurnStartResponse { turn: turn.clone() },
    );
    bridge.notify(ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: thread_id_owned.clone(),
        turn,
    }));

    bridge.spawn(async move {
        let (options, error) = settings.apply(&connection, &session_id, current_options).await;
        if let Some(error) = error {
            tracing::warn!("failed to apply turn settings: {error}");
        }
        move |bridge: &mut Bridge| {
            let Some(thread) = bridge.threads.get_mut(&thread_id_owned) else {
                return;
            };
            if let Some(options) = options {
                thread.config_options = options;
                let notification = settings_updated(thread);
                bridge.notify(notification);
            }
            let Some(thread) = bridge.threads.get_mut(&thread_id_owned) else {
                return;
            };
            // Interrupted (and already finished) while settings were applied.
            let Some(active) = thread.active.as_mut().filter(|active| active.builder.id == turn_id)
            else {
                return;
            };
            // Sent from the actor so the prompt id is recorded before any
            // event for this prompt can be processed.
            match connection.prompt(acp::PromptRequest::new(session_id, prompt)) {
                Ok(prompt_id) => active.prompt_id = Some(prompt_id),
                Err(err) => finish_turn(bridge, &thread_id_owned, Err(err)),
            }
        }
    });
}

/// Model/effort the client asked for alongside a turn or settings update.
#[derive(Default)]
pub(crate) struct TurnSettings {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<ReasoningEffort>,
}

impl TurnSettings {
    /// Apply the model, then the effort. The effort change is worked out from
    /// the options the model change returned: a new model may offer
    /// different thought levels. Returns the latest options (if any change
    /// was made) and the first error.
    pub(crate) async fn apply(
        self,
        connection: &AgentConnection,
        session_id: &acp::SessionId,
        mut options: Vec<acp::SessionConfigOption>,
    ) -> (Option<Vec<acp::SessionConfigOption>>, Option<String>) {
        let mut changed = false;
        if let Some(model) = &self.model
            && model != &models::current_model(&options)
            && let Some((config_id, value)) = models::model_value(&options, model)
        {
            match set_select(connection, session_id, config_id, value).await {
                Ok(updated) => {
                    options = updated;
                    changed = true;
                }
                Err(err) => return (changed.then_some(options), Some(err)),
            }
        }
        if let Some(effort) = &self.effort
            && Some(effort) != models::current_effort(&options).as_ref()
            && let Some((config_id, value)) = models::effort_value(&options, effort)
        {
            match set_select(connection, session_id, config_id, value).await {
                Ok(updated) => {
                    options = updated;
                    changed = true;
                }
                Err(err) => return (changed.then_some(options), Some(err)),
            }
        }
        (changed.then_some(options), None)
    }
}

async fn set_select(
    connection: &AgentConnection,
    session_id: &acp::SessionId,
    config_id: acp::SessionConfigId,
    value: String,
) -> Result<Vec<acp::SessionConfigOption>, String> {
    let request = acp::SetSessionConfigOptionRequest::new(
        session_id.clone(),
        config_id,
        acp::SessionConfigOptionValue::value_id(value),
    );
    connection
        .set_config_option(request)
        .await
        .map(|response| response.config_options)
        .map_err(|err| err.message)
}

/// `turn/interrupt` → `session/cancel`. The turn ends when the agent
/// answers the prompt with `stopReason: cancelled`.
pub(crate) fn interrupt(bridge: &mut Bridge, thread_id: &str) -> Result<(), String> {
    let Some(thread) = bridge.threads.get_mut(thread_id) else {
        return Err(format!("unknown thread {thread_id}"));
    };
    let Some(active) = thread.active.as_mut() else {
        return Ok(());
    };
    active.cancel_requested = true;
    let Some(session_id) = thread.session_id.clone() else {
        return Ok(());
    };
    let agent = thread.agent.clone();
    if let Some(connection) = bridge.agents.connection(&agent) {
        connection.cancel(&session_id).map_err(|err| err.message)?;
    }
    if bridge
        .threads
        .get(thread_id)
        .and_then(|thread| thread.active.as_ref())
        .is_some_and(|active| active.prompt_id.is_none())
    {
        // Cancelled before the prompt was even sent.
        finish_turn_with(bridge, thread_id, TurnStatus::Interrupted, None);
    }
    Ok(())
}

fn on_prompt_finished(
    bridge: &mut Bridge,
    agent: &str,
    session_id: &acp::SessionId,
    prompt_id: PromptId,
    result: Result<acp::PromptResponse, acp_client::AcpError>,
) {
    let Some(thread) = bridge.threads.by_session_mut(agent, session_id) else {
        return;
    };
    let matches = thread
        .active
        .as_ref()
        .is_some_and(|active| active.prompt_id.is_none_or(|id| id == prompt_id));
    if !matches {
        return;
    }
    let thread_id = thread.id.clone();
    finish_turn(bridge, &thread_id, result);
}

fn finish_turn(
    bridge: &mut Bridge,
    thread_id: &str,
    result: Result<acp::PromptResponse, acp_client::AcpError>,
) {
    let cancel_requested = bridge
        .threads
        .get(thread_id)
        .and_then(|thread| thread.active.as_ref())
        .is_some_and(|active| active.cancel_requested);
    match result {
        Ok(response) => {
            let (status, warning) = match response.stop_reason {
                acp::StopReason::EndTurn => (TurnStatus::Completed, None),
                acp::StopReason::Cancelled => (TurnStatus::Interrupted, None),
                acp::StopReason::MaxTokens => (
                    TurnStatus::Completed,
                    Some("The agent stopped: it reached its token limit."),
                ),
                acp::StopReason::MaxTurnRequests => (
                    TurnStatus::Completed,
                    Some("The agent stopped: it reached its request limit for this turn."),
                ),
                acp::StopReason::Refusal => (
                    TurnStatus::Completed,
                    Some("The agent refused to continue. This prompt was not added to its context."),
                ),
                _ => (TurnStatus::Completed, None),
            };
            if let Some(warning) = warning {
                bridge.notify(ServerNotification::Warning(WarningNotification {
                    thread_id: Some(thread_id.to_string()),
                    message: warning.to_string(),
                }));
            }
            finish_turn_with(bridge, thread_id, status, None);
        }
        Err(err) if cancel_requested && err.code == acp_client::AcpError::REQUEST_CANCELLED => {
            finish_turn_with(bridge, thread_id, TurnStatus::Interrupted, None);
        }
        Err(err) => {
            let message = if err.is_auth_required() {
                format!("The agent requires authentication: {}", err.message)
            } else {
                err.message
            };
            let error = records::turn_error(message);
            if let Some(turn_id) = bridge
                .threads
                .get(thread_id)
                .and_then(|thread| thread.active.as_ref())
                .map(|active| active.builder.id.clone())
            {
                bridge.notify(ServerNotification::Error(ErrorNotification {
                    error: error.clone(),
                    will_retry: false,
                    thread_id: thread_id.to_string(),
                    turn_id,
                }));
            }
            finish_turn_with(bridge, thread_id, TurnStatus::Failed, Some(error));
        }
    }
}

pub(crate) fn finish_turn_with(
    bridge: &mut Bridge,
    thread_id: &str,
    status: TurnStatus,
    error: Option<codex_app_server_protocol::TurnError>,
) {
    let Some(thread) = bridge.threads.get_mut(thread_id) else {
        return;
    };
    let Some(active) = thread.active.take() else {
        return;
    };
    let turn_id = active.builder.id.clone();
    let mut emit = Vec::new();
    let turn = active.builder.finish(status, error, &mut emit);
    thread.turns.push(turn.clone());
    thread.updated_at = records::now_secs();
    crate::approvals::abandon_turn_requests(bridge, thread_id, &turn_id);
    emit_all(bridge, emit);
    bridge.notify(ServerNotification::TurnCompleted(TurnCompletedNotification {
        thread_id: thread_id.to_string(),
        turn,
    }));
}

/// The agent process died: fail running turns and unload its sessions.
pub(crate) fn on_agent_exited(bridge: &mut Bridge, agent: &str, reason: &str, stderr: &str) {
    let detail = stderr.lines().rev().take(5).collect::<Vec<_>>();
    let mut message = format!("Agent `{agent}` exited: {reason}");
    if !detail.is_empty() {
        message.push('\n');
        message.push_str(&detail.into_iter().rev().collect::<Vec<_>>().join("\n"));
    }
    let had_threads = unload_agent_threads(bridge, agent, &message);
    if !had_threads {
        bridge.notify(ServerNotification::Warning(WarningNotification {
            thread_id: None,
            message,
        }));
    }
}

/// The agent's sessions are gone (exit or restart): withdraw its requests,
/// fail running turns, answer pending resumes, and unload its threads.
/// Returns whether the agent had any loaded thread.
pub(crate) fn unload_agent_threads(bridge: &mut Bridge, agent: &str, message: &str) -> bool {
    crate::approvals::abandon_agent_requests(bridge, agent);
    let mut any = false;
    for id in bridge.threads.on_agent(agent) {
        let had_turn = bridge
            .threads
            .get(&id)
            .is_some_and(|thread| thread.active.is_some());
        if had_turn {
            let error = records::turn_error(message.to_string());
            finish_turn_with(bridge, &id, TurnStatus::Failed, Some(error));
        }
        let Some(thread) = bridge.threads.get_mut(&id) else {
            continue;
        };
        let was_loaded = thread.loaded || thread.replay.is_some();
        thread.loaded = false;
        if let Some(reply) = thread.replay.take().and_then(|replay| replay.reply) {
            bridge.respond_error(reply.0, reply.1, crate::server::INTERNAL_ERROR, message);
        }
        if was_loaded {
            any = true;
            bridge.notify(ServerNotification::Warning(WarningNotification {
                thread_id: Some(id),
                message: message.to_string(),
            }));
        }
    }
    any
}

pub(crate) fn connection_for(bridge: &Bridge, thread: &ThreadState) -> Option<Arc<AgentConnection>> {
    bridge.agents.connection(&thread.agent)
}
