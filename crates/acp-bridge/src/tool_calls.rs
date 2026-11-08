//! ACP tool calls as Codex thread items.
//!
//! The item type is chosen once, when the call is first seen, so the TUI
//! keeps one cell per call:
//! - `execute` → `CommandExecution`
//! - `edit`/`delete`/`move`, or any call carrying a diff → `FileChange`
//! - `read` with locations → `CommandExecution` with a `Read` action, so
//!   consecutive reads collapse into the TUI's "Explored" group
//! - everything else → `DynamicToolCall` titled with the agent's title

use std::path::Path;

use acp_client::schema as acp;
use codex_app_server_protocol::CommandAction;
use codex_app_server_protocol::CommandExecutionSource;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallStatus;
use codex_app_server_protocol::FileUpdateChange;
use codex_utils_path_uri::LegacyAppPathString;
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::PatchChangeKind;
use codex_app_server_protocol::ThreadItemPayload;

use crate::content::display_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Command,
    Read,
    FileChange,
    Dynamic,
}

pub(crate) fn classify(call: &acp::ToolCall) -> ItemKind {
    let has_diff = call
        .content
        .iter()
        .any(|content| matches!(content, acp::ToolCallContent::Diff(_)));
    match call.kind {
        acp::ToolKind::Execute => ItemKind::Command,
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => ItemKind::FileChange,
        _ if has_diff => ItemKind::FileChange,
        acp::ToolKind::Read if !call.locations.is_empty() => ItemKind::Read,
        _ => ItemKind::Dynamic,
    }
}

/// Apply an update to the accumulated call (tool-calls.mdx: omitted fields
/// are unchanged; `content` and `locations` replace the collection).
pub(crate) fn apply_update(call: &mut acp::ToolCall, fields: acp::ToolCallUpdateFields) {
    if let Some(kind) = fields.kind {
        call.kind = kind;
    }
    if let Some(status) = fields.status {
        call.status = status;
    }
    if let Some(title) = fields.title {
        call.title = title;
    }
    if let Some(name) = fields.name {
        call.name = Some(name);
    }
    if let Some(content) = fields.content {
        call.content = content;
    }
    if let Some(locations) = fields.locations {
        call.locations = locations;
    }
    if let Some(raw_input) = fields.raw_input {
        call.raw_input = Some(raw_input);
    }
    if let Some(raw_output) = fields.raw_output {
        call.raw_output = Some(raw_output);
    }
}

/// A call first seen through `session/request_permission`.
pub(crate) fn from_update(update: &acp::ToolCallUpdate) -> acp::ToolCall {
    let mut call = acp::ToolCall::new(update.tool_call_id.clone(), "");
    apply_update(&mut call, update.fields.clone());
    if call.title.is_empty() {
        call.title = "Tool call".to_string();
    }
    call
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Running,
    Completed,
    Failed,
    Declined,
}

pub(crate) fn outcome(call: &acp::ToolCall) -> Outcome {
    match call.status {
        acp::ToolCallStatus::Completed => Outcome::Completed,
        acp::ToolCallStatus::Failed => Outcome::Failed,
        _ => Outcome::Running,
    }
}

/// Plain text produced by the call so far.
pub(crate) fn output_text(call: &acp::ToolCall) -> String {
    let mut parts: Vec<String> = call
        .content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Content(content) => Some(display_text(&content.content)),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect();
    if parts.is_empty()
        && let Some(raw) = &call.raw_output
    {
        match raw {
            serde_json::Value::String(text) => parts.push(text.clone()),
            serde_json::Value::Null => {}
            other => {
                if let Some(text) = ["output", "stdout", "result"]
                    .iter()
                    .find_map(|key| other.get(key).and_then(serde_json::Value::as_str))
                {
                    parts.push(text.to_string());
                }
            }
        }
    }
    parts.join("\n")
}

fn command_line(call: &acp::ToolCall) -> String {
    let from_input = call.raw_input.as_ref().and_then(|input| {
        match input.get("command") {
            Some(serde_json::Value::String(command)) => Some(command.clone()),
            Some(serde_json::Value::Array(parts)) => Some(
                parts
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        }
    });
    from_input.unwrap_or_else(|| call.title.clone())
}

fn exit_code(call: &acp::ToolCall) -> Option<i32> {
    let from_output = call.raw_output.as_ref().and_then(|output| {
        ["exit_code", "exitCode", "code"]
            .iter()
            .find_map(|key| output.get(key).and_then(serde_json::Value::as_i64))
            .and_then(|code| i32::try_from(code).ok())
    });
    from_output.or(match call.status {
        acp::ToolCallStatus::Completed => Some(0),
        acp::ToolCallStatus::Failed => Some(1),
        _ => None,
    })
}

/// The item for `call`, rendered as `kind`.
pub(crate) fn payload(
    item_id: &str,
    kind: ItemKind,
    call: &acp::ToolCall,
    outcome: Outcome,
    cwd: &Path,
    duration_ms: Option<i64>,
) -> ThreadItemPayload {
    match kind {
        ItemKind::Command | ItemKind::Read => {
            let command = command_line(call);
            let command_actions = if kind == ItemKind::Read {
                call.locations
                    .iter()
                    .map(|location| CommandAction::Read {
                        command: call.title.clone(),
                        name: location
                            .path
                            .file_name()
                            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                        path: LegacyAppPathString::from_path(&location.path),
                    })
                    .collect()
            } else {
                vec![CommandAction::Unknown {
                    command: command.clone(),
                }]
            };
            let output = output_text(call);
            ThreadItemPayload::CommandExecution {
                sandbox_type: None,
                model_context: None,
                id: item_id.to_string(),
                plugin_id: None,
                script_path: None,
                command,
                cwd: LegacyAppPathString::from_path(cwd),
                process_id: None,
                source: CommandExecutionSource::Agent,
                status: match outcome {
                    Outcome::Running => CommandExecutionStatus::InProgress,
                    Outcome::Completed => CommandExecutionStatus::Completed,
                    Outcome::Failed => CommandExecutionStatus::Failed,
                    Outcome::Declined => CommandExecutionStatus::Declined,
                },
                command_actions,
                aggregated_output: (!output.is_empty()).then_some(output),
                exit_code: if outcome == Outcome::Running {
                    None
                } else {
                    exit_code(call)
                },
                duration_ms,
            }
        }
        ItemKind::FileChange => ThreadItemPayload::FileChange {
            id: item_id.to_string(),
            changes: file_changes(call),
            status: match outcome {
                Outcome::Running => PatchApplyStatus::InProgress,
                Outcome::Completed => PatchApplyStatus::Completed,
                Outcome::Failed => PatchApplyStatus::Failed,
                Outcome::Declined => PatchApplyStatus::Declined,
            },
        },
        ItemKind::Dynamic => {
            let output = output_text(call);
            ThreadItemPayload::DynamicToolCall {
                id: item_id.to_string(),
                namespace: None,
                tool: call.title.clone(),
                arguments: call.raw_input.clone().unwrap_or(serde_json::Value::Null),
                status: match outcome {
                    Outcome::Running => DynamicToolCallStatus::InProgress,
                    Outcome::Completed => DynamicToolCallStatus::Completed,
                    Outcome::Failed | Outcome::Declined => DynamicToolCallStatus::Failed,
                },
                content_items: (outcome != Outcome::Running).then(|| {
                    if output.is_empty() {
                        Vec::new()
                    } else {
                        vec![DynamicToolCallOutputContentItem::InputText { text: output }]
                    }
                }),
                success: match outcome {
                    Outcome::Running => None,
                    Outcome::Completed => Some(true),
                    Outcome::Failed | Outcome::Declined => Some(false),
                },
                duration_ms,
            }
        }
    }
}

fn file_changes(call: &acp::ToolCall) -> Vec<FileUpdateChange> {
    let mut changes: Vec<FileUpdateChange> = call
        .content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Diff(diff) => Some(diff_change(diff)),
            _ => None,
        })
        .collect();
    if changes.is_empty() {
        // No diff yet (or a delete/move without one): list the paths touched.
        changes = call
            .locations
            .iter()
            .map(|location| FileUpdateChange {
                path: location.path.display().to_string(),
                kind: match call.kind {
                    acp::ToolKind::Delete => PatchChangeKind::Delete,
                    _ => PatchChangeKind::Update { move_path: None },
                },
                diff: String::new(),
            })
            .collect();
    }
    changes
}

fn diff_change(diff: &acp::Diff) -> FileUpdateChange {
    let path = diff.path.display().to_string();
    let (kind, unified) = match &diff.old_text {
        None => (PatchChangeKind::Add, unified_diff("", &diff.new_text, &path)),
        Some(old) => (
            PatchChangeKind::Update { move_path: None },
            unified_diff(old, &diff.new_text, &path),
        ),
    };
    FileUpdateChange {
        path,
        kind,
        diff: unified,
    }
}

/// Unified diff body (hunks only), the format Codex uses for `FileChange`.
pub(crate) fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(path, path)
        .to_string();
    // Codex stores hunks without the ---/+++ header lines.
    text.lines()
        .skip_while(|line| line.starts_with("---") || line.starts_with("+++"))
        .map(|line| format!("{line}\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn classify_by_kind_and_diff() {
        let exec = acp::ToolCall::new("1", "ls").kind(acp::ToolKind::Execute);
        assert_eq!(classify(&exec), ItemKind::Command);
        let read = acp::ToolCall::new("2", "Read a.rs")
            .kind(acp::ToolKind::Read)
            .locations(vec![acp::ToolCallLocation::new("/a.rs")]);
        assert_eq!(classify(&read), ItemKind::Read);
        let read_without_location = acp::ToolCall::new("3", "Read").kind(acp::ToolKind::Read);
        assert_eq!(classify(&read_without_location), ItemKind::Dynamic);
        let other_with_diff = acp::ToolCall::new("4", "x").content(vec![
            acp::ToolCallContent::Diff(acp::Diff::new("/a.rs", "new")),
        ]);
        assert_eq!(classify(&other_with_diff), ItemKind::FileChange);
        let search = acp::ToolCall::new("5", "grep").kind(acp::ToolKind::Search);
        assert_eq!(classify(&search), ItemKind::Dynamic);
    }

    #[test]
    fn update_patches_only_present_fields() {
        let mut call = acp::ToolCall::new("1", "Run")
            .kind(acp::ToolKind::Execute)
            .raw_input(serde_json::json!({"command": "ls"}));
        apply_update(
            &mut call,
            acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Completed),
        );
        assert_eq!(call.status, acp::ToolCallStatus::Completed);
        assert_eq!(call.title, "Run");
        assert_eq!(command_line(&call), "ls");
    }

    #[test]
    fn unified_diff_omits_file_headers() {
        let diff = unified_diff("a\nb\n", "a\nc\n", "/f.txt");
        assert_eq!(diff, "@@ -1,2 +1,2 @@\n a\n-b\n+c\n");
    }
}
