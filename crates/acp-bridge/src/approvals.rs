//! Agent-initiated requests shown to the user: permissions and elicitations.

use acp_client::PendingRequestId;
use acp_client::schema as acp;
use codex_app_server_protocol::CommandExecutionApprovalDecision;
use codex_app_server_protocol::CommandExecutionApprovalKind;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::CommandExecutionRequestApprovalResponse;
use codex_app_server_protocol::FileChangeApprovalDecision;
use codex_app_server_protocol::FileChangeRequestApprovalParams;
use codex_app_server_protocol::FileChangeRequestApprovalResponse;
use codex_app_server_protocol::McpElicitationSchema;
use codex_app_server_protocol::McpServerElicitationAction;
use codex_app_server_protocol::McpServerElicitationRequest;
use codex_app_server_protocol::McpServerElicitationRequestParams;
use codex_app_server_protocol::McpServerElicitationRequestResponse;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadItemPayload;

use crate::records;
use crate::server::Bridge;
use crate::threads::emit_all;
use crate::tool_calls::ItemKind;

/// What the user picked, independent of how it was presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    /// Stop the turn.
    Cancel,
}

/// The ACP option for a choice, falling back to the closest available kind
/// (allow_once ↔ allow_always, reject_once ↔ reject_always).
pub(crate) fn option_for(options: &[acp::PermissionOption], choice: Choice) -> Option<acp::PermissionOptionId> {
    let order: &[acp::PermissionOptionKind] = match choice {
        Choice::AllowOnce => &[acp::PermissionOptionKind::AllowOnce, acp::PermissionOptionKind::AllowAlways],
        Choice::AllowAlways => &[acp::PermissionOptionKind::AllowAlways, acp::PermissionOptionKind::AllowOnce],
        Choice::RejectOnce => &[acp::PermissionOptionKind::RejectOnce, acp::PermissionOptionKind::RejectAlways],
        Choice::RejectAlways => &[acp::PermissionOptionKind::RejectAlways, acp::PermissionOptionKind::RejectOnce],
        Choice::Cancel => &[],
    };
    order.iter().find_map(|kind| {
        options
            .iter()
            .find(|option| option.kind == *kind)
            .map(|option| option.option_id.clone())
    })
}

fn has_kind(options: &[acp::PermissionOption], kind: acp::PermissionOptionKind) -> bool {
    options.iter().any(|option| option.kind == kind)
}

fn command_decisions(options: &[acp::PermissionOption]) -> Vec<CommandExecutionApprovalDecision> {
    let mut decisions = Vec::new();
    if has_kind(options, acp::PermissionOptionKind::AllowOnce) {
        decisions.push(CommandExecutionApprovalDecision::Accept);
    }
    if has_kind(options, acp::PermissionOptionKind::AllowAlways) {
        decisions.push(CommandExecutionApprovalDecision::AcceptForSession);
    }
    if decisions.is_empty() && !options.is_empty() {
        decisions.push(CommandExecutionApprovalDecision::Accept);
    }
    decisions.push(CommandExecutionApprovalDecision::Decline);
    decisions.push(CommandExecutionApprovalDecision::Cancel);
    decisions
}

pub(crate) fn on_permission(
    bridge: &mut Bridge,
    agent: &str,
    id: PendingRequestId,
    request: acp::RequestPermissionRequest,
) {
    let connection = bridge.agents.connection(agent);
    let cancel = |connection: &Option<std::sync::Arc<acp_client::AgentConnection>>| {
        if let Some(connection) = connection {
            connection.respond_permission(id, acp::RequestPermissionOutcome::Cancelled);
        }
    };
    let Some(thread) = bridge.threads.by_session_mut(agent, &request.session_id) else {
        cancel(&connection);
        return;
    };
    let thread_id = thread.id.clone();
    let cwd = thread.cwd.clone();
    let Some(active) = thread.active.as_mut() else {
        // Permissions only make sense inside a prompt turn.
        cancel(&connection);
        return;
    };
    let mut emit = Vec::new();
    let (item_id, kind) = active.builder.tool_item(&request.tool_call, &mut emit);
    let turn_id = active.builder.id.clone();
    let started_at_ms = records::now_ms();
    let title = active
        .builder
        .tool_title(&request.tool_call.tool_call_id)
        .unwrap_or_else(|| "Run tool".to_string());
    emit_all(bridge, emit);

    let options = request.options.clone();
    let agent_owned = agent.to_string();
    let request_id = match kind {
        ItemKind::FileChange => {
            let params = FileChangeRequestApprovalParams {
                thread_id: thread_id.clone(),
                turn_id,
                item_id,
                started_at_ms,
                reason: Some(title),
                grant_root: None,
            };
            bridge.request(
                Some(thread_id.clone()),
                |request_id| ServerRequest::FileChangeRequestApproval { request_id, params },
                Box::new(move |bridge, result| {
                    let choice = result
                        .ok()
                        .and_then(|value| serde_json::from_value::<FileChangeRequestApprovalResponse>(value).ok())
                        .map_or(Choice::Cancel, |response| match response.decision {
                            FileChangeApprovalDecision::Accept => Choice::AllowOnce,
                            FileChangeApprovalDecision::AcceptForSession => Choice::AllowAlways,
                            FileChangeApprovalDecision::Decline => Choice::RejectOnce,
                            FileChangeApprovalDecision::Cancel => Choice::Cancel,
                        });
                    answer(bridge, &agent_owned, id, &options, choice);
                }),
            )
        }
        ItemKind::Command | ItemKind::Read | ItemKind::Dynamic => {
            let command = match bridge
                .threads
                .get(&thread_id)
                .and_then(|thread| thread.active.as_ref())
                .and_then(|active| active.builder.item_payload(&item_id))
            {
                Some(ThreadItemPayload::CommandExecution { command, .. }) => command,
                _ => title.clone(),
            };
            let params = CommandExecutionRequestApprovalParams {
                kind: CommandExecutionApprovalKind::Command,
                thread_id: thread_id.clone(),
                turn_id,
                item_id,
                started_at_ms,
                approval_id: None,
                environment_id: None,
                reason: (title != command).then_some(title),
                network_approval_context: None,
                command: Some(command),
                cwd: Some(codex_utils_path_uri::LegacyAppPathString::from_abs_path(&cwd)),
                command_actions: None,
                additional_permissions: None,
                proposed_execpolicy_amendment: None,
                proposed_network_policy_amendments: None,
                available_decisions: Some(command_decisions(&options)),
            };
            bridge.request(
                Some(thread_id.clone()),
                |request_id| ServerRequest::CommandExecutionRequestApproval { request_id, params },
                Box::new(move |bridge, result| {
                    let choice = result
                        .ok()
                        .and_then(|value| {
                            serde_json::from_value::<CommandExecutionRequestApprovalResponse>(value).ok()
                        })
                        .map_or(Choice::Cancel, |response| match response.decision {
                            CommandExecutionApprovalDecision::Accept
                            | CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment { .. }
                            | CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment { .. } => {
                                Choice::AllowOnce
                            }
                            CommandExecutionApprovalDecision::AcceptForSession => Choice::AllowAlways,
                            CommandExecutionApprovalDecision::Decline => Choice::RejectOnce,
                            CommandExecutionApprovalDecision::Cancel => Choice::Cancel,
                        });
                    answer(bridge, &agent_owned, id, &options, choice);
                }),
            )
        }
    };
    if let Some(active) = bridge
        .threads
        .get_mut(&thread_id)
        .and_then(|thread| thread.active.as_mut())
    {
        active.pending.insert(id, request_id);
    }
}

/// Send the user's choice to the agent.
pub(crate) fn answer(
    bridge: &mut Bridge,
    agent: &str,
    id: PendingRequestId,
    options: &[acp::PermissionOption],
    choice: Choice,
) {
    forget_pending(bridge, agent, id);
    let Some(connection) = bridge.agents.connection(agent) else {
        return;
    };
    let outcome = match option_for(options, choice) {
        Some(option_id) => {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id))
        }
        None => acp::RequestPermissionOutcome::Cancelled,
    };
    if choice == Choice::Cancel {
        // "Stop": decline this request, then cancel the turn it belongs to.
        let session = bridge
            .threads
            .on_agent(agent)
            .into_iter()
            .find_map(|thread_id| {
                let thread = bridge.threads.get(&thread_id)?;
                let active = thread.active.as_ref()?;
                active.pending.contains_key(&id).then(|| thread_id.clone())
            });
        connection.respond_permission(id, acp::RequestPermissionOutcome::Cancelled);
        if let Some(thread_id) = session {
            let _ = crate::threads::interrupt(bridge, &thread_id);
        }
        return;
    }
    connection.respond_permission(id, outcome);
}

fn forget_pending(bridge: &mut Bridge, agent: &str, id: PendingRequestId) {
    for thread_id in bridge.threads.on_agent(agent) {
        if let Some(active) = bridge
            .threads
            .get_mut(&thread_id)
            .and_then(|thread| thread.active.as_mut())
        {
            active.pending.remove(&id);
        }
    }
}

pub(crate) fn on_pending_resolved(bridge: &mut Bridge, agent: &str, id: PendingRequestId) {
    for thread_id in bridge.threads.on_agent(agent) {
        let request_id = bridge
            .threads
            .get_mut(&thread_id)
            .and_then(|thread| thread.active.as_mut())
            .and_then(|active| active.pending.remove(&id));
        if let Some(request_id) = request_id {
            bridge.resolve_server_request(&request_id);
        }
    }
}

fn elicitation_session(request: &acp::CreateElicitationRequest) -> Option<acp::SessionId> {
    let scope = match &request.mode {
        acp::ElicitationMode::Form(form) => &form.scope,
        acp::ElicitationMode::Url(url) => &url.scope,
        _ => return None,
    };
    match scope {
        acp::ElicitationScope::Session(scope) => Some(scope.session_id.clone()),
        _ => None,
    }
}

pub(crate) fn on_elicitation(
    bridge: &mut Bridge,
    agent: &str,
    id: PendingRequestId,
    request: acp::CreateElicitationRequest,
) {
    let decline = |bridge: &Bridge| {
        if let Some(connection) = bridge.agents.connection(agent) {
            connection.respond_elicitation(
                id,
                acp::CreateElicitationResponse::new(acp::ElicitationAction::Decline),
            );
        }
    };
    // Session-scoped requests belong to their thread; request-scoped ones
    // (no session) are shown on any loaded thread of this agent.
    let thread_id = match elicitation_session(&request) {
        Some(session_id) => bridge
            .threads
            .by_session_mut(agent, &session_id)
            .map(|thread| thread.id.clone()),
        None => bridge
            .threads
            .on_agent(agent)
            .into_iter()
            .find(|thread_id| bridge.threads.get(thread_id).is_some_and(|t| t.loaded)),
    };
    let Some(thread_id) = thread_id else {
        decline(bridge);
        return;
    };
    let turn_id = bridge
        .threads
        .get(&thread_id)
        .and_then(|thread| thread.active.as_ref())
        .map(|active| active.builder.id.clone());
    let server_name = bridge
        .agents
        .profile(agent)
        .map_or_else(|| agent.to_string(), |profile| profile.name.clone());
    let elicitation = match &request.mode {
        acp::ElicitationMode::Form(form) => {
            let schema = serde_json::to_value(&form.requested_schema)
                .ok()
                .and_then(|value| serde_json::from_value::<McpElicitationSchema>(value).ok());
            let Some(requested_schema) = schema else {
                decline(bridge);
                return;
            };
            McpServerElicitationRequest::Form {
                meta: None,
                message: request.message.clone(),
                requested_schema,
            }
        }
        acp::ElicitationMode::Url(url) => McpServerElicitationRequest::Url {
            meta: None,
            message: request.message.clone(),
            url: url.url.clone(),
            elicitation_id: url.elicitation_id.to_string(),
        },
        _ => {
            decline(bridge);
            return;
        }
    };
    let params = McpServerElicitationRequestParams {
        thread_id: thread_id.clone(),
        turn_id,
        server_name,
        request: elicitation,
    };
    let agent_owned = agent.to_string();
    let request_id = bridge.request(
        Some(thread_id.clone()),
        |request_id| ServerRequest::McpServerElicitationRequest { request_id, params },
        Box::new(move |bridge, result| {
            forget_pending(bridge, &agent_owned, id);
            let action = result
                .ok()
                .and_then(|value| serde_json::from_value::<McpServerElicitationRequestResponse>(value).ok())
                .map_or(acp::ElicitationAction::Cancel, |response| match response.action {
                    McpServerElicitationAction::Accept => {
                        acp::ElicitationAction::Accept(acp::ElicitationAcceptAction::new().content(
                            response
                                .content
                                .and_then(|content| serde_json::from_value(content).ok()),
                        ))
                    }
                    McpServerElicitationAction::Decline => acp::ElicitationAction::Decline,
                    McpServerElicitationAction::Cancel => acp::ElicitationAction::Cancel,
                });
            if let Some(connection) = bridge.agents.connection(&agent_owned) {
                connection.respond_elicitation(id, acp::CreateElicitationResponse::new(action));
            }
        }),
    );
    if let Some(active) = bridge
        .threads
        .get_mut(&thread_id)
        .and_then(|thread| thread.active.as_mut())
    {
        active.pending.insert(id, request_id);
    }
}

pub(crate) fn on_elicitation_complete(
    _bridge: &mut Bridge,
    agent: &str,
    notification: &acp::CompleteElicitationNotification,
) {
    tracing::debug!(%agent, elicitation = %notification.elicitation_id, "url elicitation completed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn option(id: &str, kind: acp::PermissionOptionKind) -> acp::PermissionOption {
        acp::PermissionOption::new(id.to_string(), id, kind)
    }

    #[test]
    fn choices_fall_back_to_the_nearest_kind() {
        let options = vec![
            option("yes", acp::PermissionOptionKind::AllowOnce),
            option("no-forever", acp::PermissionOptionKind::RejectAlways),
        ];
        assert_eq!(option_for(&options, Choice::AllowAlways).map(|o| o.to_string()), Some("yes".into()));
        assert_eq!(option_for(&options, Choice::RejectOnce).map(|o| o.to_string()), Some("no-forever".into()));
        assert_eq!(option_for(&options, Choice::Cancel), None);
        assert_eq!(
            command_decisions(&options),
            vec![
                CommandExecutionApprovalDecision::Accept,
                CommandExecutionApprovalDecision::Decline,
                CommandExecutionApprovalDecision::Cancel,
            ]
        );
    }
}
