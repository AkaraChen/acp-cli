//! Agent-initiated requests shown to the user: permissions and elicitations.

use acp_client::PendingRequestId;
use acp_client::schema as acp;
use codex_app_server_protocol::AcpcPermissionOption;
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
use crate::threads::PendingKind;
use crate::tool_calls::ItemKind;

/// What the user picked, independent of how it was presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Choice {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    /// One of the agent's options, picked by id.
    Option(acp::PermissionOptionId),
    /// Stop the turn.
    Cancel,
}

/// The ACP option for a choice, falling back to the closest available kind
/// (allow_once ↔ allow_always, reject_once ↔ reject_always).
pub(crate) fn option_for(options: &[acp::PermissionOption], choice: &Choice) -> Option<acp::PermissionOptionId> {
    let order: &[acp::PermissionOptionKind] = match choice {
        Choice::AllowOnce => &[acp::PermissionOptionKind::AllowOnce, acp::PermissionOptionKind::AllowAlways],
        Choice::AllowAlways => &[acp::PermissionOptionKind::AllowAlways, acp::PermissionOptionKind::AllowOnce],
        Choice::RejectOnce => &[acp::PermissionOptionKind::RejectOnce, acp::PermissionOptionKind::RejectAlways],
        Choice::Option(option_id) => {
            return options
                .iter()
                .any(|option| &option.option_id == option_id)
                .then(|| option_id.clone());
        }
        Choice::Cancel => &[],
    };
    order.iter().find_map(|kind| {
        options
            .iter()
            .find(|option| option.kind == *kind)
            .map(|option| option.option_id.clone())
    })
}

fn kind_name(kind: &acp::PermissionOptionKind) -> &'static str {
    match kind {
        acp::PermissionOptionKind::AllowOnce => "allow_once",
        acp::PermissionOptionKind::AllowAlways => "allow_always",
        acp::PermissionOptionKind::RejectOnce => "reject_once",
        acp::PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

/// The agent's own options, shown with their own labels (patches/0004),
/// plus Codex's "stop and tell the agent" choice.
fn command_decisions(options: &[acp::PermissionOption]) -> Vec<CommandExecutionApprovalDecision> {
    options
        .iter()
        .map(|option| CommandExecutionApprovalDecision::AcpcOption {
            option: AcpcPermissionOption {
                option_id: option.option_id.to_string(),
                name: option.name.clone(),
                kind: kind_name(&option.kind).to_string(),
            },
        })
        .chain(std::iter::once(CommandExecutionApprovalDecision::Cancel))
        .collect()
}

/// Whether Codex's fixed file-change choices (yes / yes, don't ask again /
/// no) say exactly what the agent offers: one option of each kind.
fn fits_file_change_overlay(options: &[acp::PermissionOption]) -> bool {
    let count = |kind: acp::PermissionOptionKind| options.iter().filter(|option| option.kind == kind).count();
    let rejects = count(acp::PermissionOptionKind::RejectOnce) + count(acp::PermissionOptionKind::RejectAlways);
    count(acp::PermissionOptionKind::AllowOnce) == 1
        && count(acp::PermissionOptionKind::AllowAlways) == 1
        && rejects == 1
        && options.len() == 3
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
    if active.cancel_requested {
        // prompt-turn.mdx: once cancelled, every permission is `cancelled`.
        cancel(&connection);
        return;
    }
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
        ItemKind::FileChange if fits_file_change_overlay(&options) => {
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
                    answer(bridge, &agent_owned, id, &options, &choice);
                }),
            )
        }
        // Other calls, and file changes whose options the file overlay
        // cannot express, use the command overlay with the agent's options.
        ItemKind::FileChange | ItemKind::Command | ItemKind::Read | ItemKind::Dynamic => {
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
                            CommandExecutionApprovalDecision::AcpcOption { option } => {
                                Choice::Option(acp::PermissionOptionId::new(option.option_id))
                            }
                        });
                    answer(bridge, &agent_owned, id, &options, &choice);
                }),
            )
        }
    };
    if let Some(active) = bridge
        .threads
        .get_mut(&thread_id)
        .and_then(|thread| thread.active.as_mut())
    {
        active.pending.insert(id, (request_id, PendingKind::Permission));
    }
}

/// Send the user's choice to the agent.
pub(crate) fn answer(
    bridge: &mut Bridge,
    agent: &str,
    id: PendingRequestId,
    options: &[acp::PermissionOption],
    choice: &Choice,
) {
    // The thread must be found before the request is forgotten.
    let thread_id = bridge.threads.on_agent(agent).into_iter().find(|thread_id| {
        bridge
            .threads
            .get(thread_id)
            .and_then(|thread| thread.active.as_ref())
            .is_some_and(|active| active.pending.contains_key(&id))
    });
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
    if *choice == Choice::Cancel {
        // "Stop": answer this request cancelled, then cancel its turn.
        connection.respond_permission(id, acp::RequestPermissionOutcome::Cancelled);
        if let Some(thread_id) = thread_id {
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
        if let Some((request_id, _)) = request_id {
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
    // A request the user never saw is cancelled, never declined on their
    // behalf.
    let cancel = |bridge: &Bridge| {
        if let Some(connection) = bridge.agents.connection(agent) {
            connection.respond_elicitation(
                id,
                acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel),
            );
        }
    };
    // Session-scoped requests belong to their thread. Request-scoped ones
    // (sign-in, setup) have none; the TUI only shows requests on a thread,
    // so they go to the most recently active loaded thread.
    let thread_id = match elicitation_session(&request) {
        Some(session_id) => bridge
            .threads
            .by_session_mut(agent, &session_id)
            .filter(|thread| {
                !thread
                    .active
                    .as_ref()
                    .is_some_and(|active| active.cancel_requested)
            })
            .map(|thread| thread.id.clone()),
        None => bridge.threads.most_recent_loaded(),
    };
    let Some(thread_id) = thread_id else {
        cancel(bridge);
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
            let Some((requested_schema, message)) =
                mcp_schema(&form.requested_schema, &request.message)
            else {
                tracing::warn!(%agent, "elicitation schema cannot be shown; cancelling");
                cancel(bridge);
                return;
            };
            McpServerElicitationRequest::Form {
                meta: None,
                message,
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
            cancel(bridge);
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
        active.pending.insert(id, (request_id, PendingKind::Elicitation));
    }
}

/// The ACP form schema as the TUI's MCP-style schema. ACP adds a few
/// presentation-only keys MCP rejects (schema title/description, string
/// patterns, `_meta`, option descriptions): the schema text moves into the
/// message and the rest is dropped.
fn mcp_schema(
    schema: &acp::ElicitationSchema,
    message: &str,
) -> Option<(McpElicitationSchema, String)> {
    let mut value = serde_json::to_value(schema).ok()?;
    let object = value.as_object_mut()?;
    let mut message = message.to_string();
    for key in ["title", "description"] {
        if let Some(text) = object.remove(key).as_ref().and_then(serde_json::Value::as_str)
            && !text.is_empty()
            && !message.contains(text)
        {
            message.push_str("\n\n");
            message.push_str(text);
        }
    }
    object.remove("_meta");
    if let Some(properties) = object
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
    {
        for property in properties.values_mut() {
            strip_property(property);
        }
    }
    let schema = serde_json::from_value::<McpElicitationSchema>(value).ok()?;
    Some((schema, message))
}

fn strip_property(property: &mut serde_json::Value) {
    let Some(object) = property.as_object_mut() else {
        return;
    };
    object.remove("_meta");
    object.remove("pattern");
    for key in ["oneOf", "anyOf"] {
        if let Some(options) = object.get_mut(key).and_then(serde_json::Value::as_array_mut) {
            for option in options {
                if let Some(option) = option.as_object_mut() {
                    option.remove("description");
                    option.remove("_meta");
                }
            }
        }
    }
    if let Some(items) = object.get_mut("items") {
        strip_property(items);
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
        assert_eq!(option_for(&options, &Choice::AllowAlways).map(|o| o.to_string()), Some("yes".into()));
        assert_eq!(option_for(&options, &Choice::RejectOnce).map(|o| o.to_string()), Some("no-forever".into()));
        assert_eq!(option_for(&options, &Choice::Cancel), None);
        assert_eq!(
            option_for(&options, &Choice::Option(acp::PermissionOptionId::new("no-forever")))
                .map(|o| o.to_string()),
            Some("no-forever".into())
        );
        assert_eq!(option_for(&options, &Choice::Option(acp::PermissionOptionId::new("gone"))), None);
    }

    #[test]
    fn command_decisions_carry_the_agents_options() {
        let options = vec![
            option("a", acp::PermissionOptionKind::AllowAlways),
            option("b", acp::PermissionOptionKind::AllowAlways),
        ];
        let decisions = command_decisions(&options);
        assert_eq!(decisions.len(), 3);
        assert_eq!(decisions[2], CommandExecutionApprovalDecision::Cancel);
        assert!(matches!(
            &decisions[1],
            CommandExecutionApprovalDecision::AcpcOption { option } if option.option_id == "b" && option.kind == "allow_always"
        ));
    }

    #[test]
    fn file_overlay_needs_one_option_of_each_kind() {
        let canonical = vec![
            option("y", acp::PermissionOptionKind::AllowOnce),
            option("a", acp::PermissionOptionKind::AllowAlways),
            option("n", acp::PermissionOptionKind::RejectOnce),
        ];
        assert!(fits_file_change_overlay(&canonical));
        assert!(!fits_file_change_overlay(&canonical[..2]));
        let mut doubled = canonical.clone();
        doubled.push(option("a2", acp::PermissionOptionKind::AllowAlways));
        assert!(!fits_file_change_overlay(&doubled));
    }
}
