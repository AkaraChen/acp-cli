//! The `acpc/*` protocol extension (patches/0002): ACP concepts the stock
//! app-server protocol cannot carry.

use acp_client::schema as acp;
use codex_app_server_protocol::AcpcAgent;
use codex_app_server_protocol::AcpcAgentsUpdatedNotification;
use codex_app_server_protocol::AcpcAvailableCommand;
use codex_app_server_protocol::AcpcAvailableCommandsUpdatedNotification;
use codex_app_server_protocol::AcpcNoticeNotification;
use codex_app_server_protocol::AcpcSessionConfigKind;
use codex_app_server_protocol::AcpcSessionConfigOption;
use codex_app_server_protocol::AcpcSessionConfigUpdatedNotification;
use codex_app_server_protocol::AcpcSessionConfigValue;
use codex_app_server_protocol::AcpcSessionMode;
use codex_app_server_protocol::AcpcSessionModes;
use codex_app_server_protocol::ServerNotification;

use crate::models;
use crate::server::Bridge;
use crate::threads::ThreadState;
use crate::turn::Emit;

/// `thread/start` config key selecting the agent for a new thread.
pub const AGENT_CONFIG_KEY: &str = "acpc.agent";

pub(crate) fn commands_updated(thread: &ThreadState, emit: &mut Emit) {
    emit.push(ServerNotification::AcpcAvailableCommandsUpdated(
        AcpcAvailableCommandsUpdatedNotification {
            thread_id: thread.id.clone(),
            commands: thread
                .commands
                .iter()
                .map(|command| AcpcAvailableCommand {
                    name: command.name.clone(),
                    description: command.description.clone(),
                    input_hint: match &command.input {
                        Some(acp::AvailableCommandInput::Unstructured(input)) => {
                            Some(input.hint.clone())
                        }
                        _ => None,
                    },
                })
                .collect(),
        },
    ));
}

/// Ids of the options `/model` already presents (the model selector and
/// thought level, via `model/list`); `/options` shows the rest.
fn model_option_ids(options: &[acp::SessionConfigOption]) -> Vec<acp::SessionConfigId> {
    [
        acp::SessionConfigOptionCategory::Model,
        acp::SessionConfigOptionCategory::ThoughtLevel,
    ]
    .iter()
    .filter_map(|category| models::option_by_category(options, category))
    .map(|option| option.id.clone())
    .collect()
}

fn category_name(category: &acp::SessionConfigOptionCategory) -> String {
    match category {
        acp::SessionConfigOptionCategory::Mode => "mode".to_string(),
        acp::SessionConfigOptionCategory::Model => "model".to_string(),
        acp::SessionConfigOptionCategory::ModelConfig => "model_config".to_string(),
        acp::SessionConfigOptionCategory::ThoughtLevel => "thought_level".to_string(),
        acp::SessionConfigOptionCategory::Other(other) => other.clone(),
        _ => "other".to_string(),
    }
}

fn config_option(option: &acp::SessionConfigOption) -> Option<AcpcSessionConfigOption> {
    let kind = match &option.kind {
        acp::SessionConfigKind::Select(select) => {
            let options = match &select.options {
                acp::SessionConfigSelectOptions::Ungrouped(values) => values
                    .iter()
                    .map(|value| AcpcSessionConfigValue {
                        value: value.value.to_string(),
                        name: value.name.clone(),
                        description: value.description.clone(),
                        group: None,
                    })
                    .collect(),
                acp::SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|group| {
                        group.options.iter().map(|value| AcpcSessionConfigValue {
                            value: value.value.to_string(),
                            name: value.name.clone(),
                            description: value.description.clone(),
                            group: Some(group.name.clone()),
                        })
                    })
                    .collect(),
                _ => return None,
            };
            AcpcSessionConfigKind::Select {
                current_value: select.current_value.to_string(),
                options,
            }
        }
        acp::SessionConfigKind::Boolean(boolean) => AcpcSessionConfigKind::Boolean {
            current_value: boolean.current_value,
        },
        // session-config-options.mdx: ignore options of unknown type.
        _ => return None,
    };
    Some(AcpcSessionConfigOption {
        id: option.id.to_string(),
        name: option.name.clone(),
        description: option.description.clone(),
        category: option.category.as_ref().map(category_name),
        kind,
    })
}

pub(crate) fn session_config_updated(thread: &ThreadState, emit: &mut Emit) {
    let hidden = model_option_ids(&thread.config_options);
    let options = thread
        .config_options
        .iter()
        .filter(|option| !hidden.contains(&option.id))
        .filter_map(config_option)
        .collect();
    // Legacy modes only when the agent has no config options (they would
    // otherwise duplicate the `mode` category).
    let modes = if thread.config_options.is_empty() {
        thread.modes.as_ref().map(|modes| AcpcSessionModes {
            current_mode_id: modes.current_mode_id.to_string(),
            available: modes
                .available_modes
                .iter()
                .map(|mode| AcpcSessionMode {
                    id: mode.id.to_string(),
                    name: mode.name.clone(),
                    description: mode.description.clone(),
                })
                .collect(),
        })
    } else {
        None
    };
    emit.push(ServerNotification::AcpcSessionConfigUpdated(
        AcpcSessionConfigUpdatedNotification {
            thread_id: thread.id.clone(),
            options,
            modes,
        },
    ));
}

pub(crate) fn notice(thread_id: &str, notice: &acp::Notice, emit: &mut Emit) {
    let severity = match &notice.severity {
        acp::NoticeSeverity::Info => "info".to_string(),
        acp::NoticeSeverity::Warning => "warning".to_string(),
        acp::NoticeSeverity::Error => "error".to_string(),
        acp::NoticeSeverity::Other(other) => other.clone(),
        _ => "info".to_string(),
    };
    emit.push(ServerNotification::AcpcNotice(AcpcNoticeNotification {
        thread_id: Some(thread_id.to_string()),
        severity,
        title: notice.title.clone(),
        description: notice.description.clone(),
    }));
}

pub(crate) fn agents(bridge: &Bridge) -> Vec<AcpcAgent> {
    bridge
        .agents
        .profiles()
        .iter()
        .map(|profile| AcpcAgent {
            slug: profile.slug.clone(),
            name: profile.name.clone(),
        })
        .collect()
}

pub(crate) fn agents_updated(bridge: &Bridge) {
    bridge.notify(ServerNotification::AcpcAgentsUpdated(
        AcpcAgentsUpdatedNotification {
            agents: agents(bridge),
            current: bridge.agents.current.clone(),
        },
    ));
}

/// Resend everything the TUI mirrors for `thread` (its chat widget is
/// rebuilt on every new/resumed session).
pub(crate) fn thread_state(bridge: &Bridge, thread: &ThreadState) {
    agents_updated(bridge);
    let mut emit = Vec::new();
    session_config_updated(thread, &mut emit);
    commands_updated(thread, &mut emit);
    if let Some((used, size)) = thread.usage {
        let turn_id = thread
            .active
            .as_ref()
            .map(|active| active.builder.id.clone())
            .or_else(|| thread.turns.last().map(|turn| turn.id.clone()))
            .unwrap_or_default();
        emit.push(crate::threads::usage_updated(&thread.id, &turn_id, used, size));
    }
    crate::threads::emit_all(bridge, emit);
}
