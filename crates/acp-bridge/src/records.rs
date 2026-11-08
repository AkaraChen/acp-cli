//! Construction of the app-server records the TUI expects.
//!
//! These structs have no `Default` and many Codex-only fields. Building
//! them in one place keeps upstream bumps to a single file.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_app_server_protocol::ApprovalsReviewer;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::Model;
use codex_app_server_protocol::ReasoningEffortOption;
use codex_app_server_protocol::SandboxPolicy;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSettings;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnError;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStatus;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::MultiAgentMode;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ReasoningEffort;

use crate::threads::ThreadState;

pub(crate) const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

pub(crate) fn now_secs() -> i64 {
    now_ms() / 1000
}

pub(crate) fn item(payload: ThreadItemPayload, started_at_ms: i64) -> ThreadItem {
    ThreadItem {
        payload,
        started_at_ms: Some(started_at_ms),
        completed_at_ms: None,
    }
}

pub(crate) fn turn(
    id: &str,
    items: Vec<ThreadItem>,
    status: TurnStatus,
    error: Option<TurnError>,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
) -> Turn {
    let duration_ms = match (started_at_ms, completed_at_ms) {
        (Some(start), Some(end)) => Some(end.saturating_sub(start)),
        _ => None,
    };
    Turn {
        id: id.to_string(),
        root_turn_id: None,
        items,
        items_view: TurnItemsView::Full,
        status,
        error,
        started_at: started_at_ms.map(|ms| ms / 1000),
        completed_at: completed_at_ms.map(|ms| ms / 1000),
        duration_ms,
    }
}

pub(crate) fn turn_error(message: impl Into<String>) -> TurnError {
    TurnError {
        message: message.into(),
        codex_error_info: None,
        additional_details: None,
        misalignment: None,
    }
}

pub(crate) fn thread(state: &ThreadState, include_turns: bool) -> Thread {
    Thread {
        id: state.id.clone(),
        environments: None,
        extra: None,
        session_id: state.id.clone(),
        forked_from_id: None,
        parent_thread_id: None,
        preview: state.preview.clone(),
        ephemeral: false,
        section: None,
        section_entered_at: None,
        project_id: None,
        history_mode: ThreadHistoryMode::Legacy,
        model_provider: state.agent.clone(),
        model: Some(state.model_label()),
        reasoning_effort: None,
        created_at: state.created_at,
        updated_at: state.updated_at,
        recency_at: Some(state.updated_at),
        status: if state.session_id.is_some() {
            ThreadStatus::Idle
        } else {
            ThreadStatus::NotLoaded
        },
        path: None,
        cwd: state.cwd.clone(),
        cli_version: CLI_VERSION.to_string(),
        originator: Some("acpc".to_string()),
        source: SessionSource::Cli,
        can_accept_direct_input: None,
        thread_source: None,
        agent_nickname: None,
        agent_role: None,
        git_info: None,
        name: state.title.clone(),
        daybreak_enabled: None,
        turns: if include_turns {
            state.turns.clone()
        } else {
            Vec::new()
        },
    }
}

/// The policy fields are Codex sandbox concepts. ACP agents enforce their
/// own permissions, so report the most permissive client-side policy.
fn approval_policy() -> AskForApproval {
    AskForApproval::Never
}

fn sandbox_policy() -> SandboxPolicy {
    SandboxPolicy::DangerFullAccess
}

pub(crate) fn thread_start_response(state: &ThreadState) -> ThreadStartResponse {
    ThreadStartResponse {
        thread: thread(state, false),
        model: state.model_label(),
        model_provider: state.agent.clone(),
        service_tier: None,
        disabled_plugin_ids: Vec::new(),
        cwd: state.cwd.clone(),
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: approval_policy(),
        approvals_reviewer: ApprovalsReviewer::User,
        sandbox: sandbox_policy(),
        active_permission_profile: None,
        reasoning_effort: state.reasoning_effort(),
        multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
    }
}

pub(crate) fn thread_resume_response(state: &ThreadState) -> ThreadResumeResponse {
    ThreadResumeResponse {
        thread: thread(state, true),
        model: state.model_label(),
        model_provider: state.agent.clone(),
        service_tier: None,
        disabled_plugin_ids: Vec::new(),
        cwd: state.cwd.clone(),
        runtime_workspace_roots: Vec::new(),
        instruction_sources: Vec::new(),
        approval_policy: approval_policy(),
        approvals_reviewer: ApprovalsReviewer::User,
        sandbox: sandbox_policy(),
        active_permission_profile: None,
        reasoning_effort: state.reasoning_effort(),
        collaboration_mode: None,
        multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
        initial_turns_page: None,
        turns_backwards_cursor: None,
        items_backwards_cursor: None,
    }
}

pub(crate) fn thread_settings(state: &ThreadState) -> ThreadSettings {
    let model = state.model_label();
    ThreadSettings {
        disabled_plugin_ids: Vec::new(),
        cwd: state.cwd.clone(),
        approval_policy: approval_policy(),
        approvals_reviewer: ApprovalsReviewer::User,
        sandbox_policy: sandbox_policy(),
        active_permission_profile: None,
        model: model.clone(),
        model_provider: state.agent.clone(),
        service_tier: None,
        effort: state.reasoning_effort(),
        summary: None,
        collaboration_mode: CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model,
                reasoning_effort: state.reasoning_effort(),
                developer_instructions: None,
            },
        },
        multi_agent_mode: MultiAgentMode::ExplicitRequestOnly,
        personality: None,
    }
}

/// One entry for `model/list`.
pub(crate) struct ModelEntry<'a> {
    pub id: &'a str,
    pub display_name: &'a str,
    pub description: &'a str,
    pub is_default: bool,
    pub efforts: Vec<(ReasoningEffort, String)>,
    pub default_effort: Option<ReasoningEffort>,
    pub image_input: bool,
}

pub(crate) fn model(entry: ModelEntry<'_>) -> Model {
    let mut input_modalities = vec![InputModality::Text];
    if entry.image_input {
        input_modalities.push(InputModality::Image);
    }
    let default_reasoning_effort = entry
        .default_effort
        .or_else(|| entry.efforts.first().map(|(effort, _)| effort.clone()))
        .unwrap_or(ReasoningEffort::Medium);
    Model {
        id: entry.id.to_string(),
        model: entry.id.to_string(),
        upgrade: None,
        upgrade_info: None,
        availability_nux: None,
        display_name: entry.display_name.to_string(),
        description: entry.description.to_string(),
        model_specialty: None,
        hidden: false,
        supported_reasoning_efforts: entry
            .efforts
            .into_iter()
            .map(|(reasoning_effort, description)| ReasoningEffortOption {
                reasoning_effort,
                description,
            })
            .collect(),
        default_reasoning_effort,
        input_modalities,
        supports_personality: false,
        multi_agent_version: None,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        available_access_programs: None,
        is_default: entry.is_default,
    }
}

pub(crate) fn absolute(path: &std::path::Path) -> Option<codex_utils_absolute_path::AbsolutePathBuf> {
    codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(path).ok()
}
