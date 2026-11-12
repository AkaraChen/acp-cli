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
use codex_app_server_protocol::PatchApplyStatus;
use codex_app_server_protocol::PatchChangeKind;
use codex_app_server_protocol::ThreadItemPayload;
use codex_utils_path_uri::LegacyAppPathString;

use crate::content::display_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemKind {
    Command,
    Read,
    FileChange,
    Dynamic,
}

fn has_diff(content: &[acp::ToolCallContent]) -> bool {
    content
        .iter()
        .any(|content| matches!(content, acp::ToolCallContent::Diff(_)))
}

pub(crate) fn classify(call: &acp::ToolCall) -> ItemKind {
    match call.kind {
        acp::ToolKind::Execute => ItemKind::Command,
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move => ItemKind::FileChange,
        _ if has_diff(&call.content) => ItemKind::FileChange,
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
        // A later update without a diff (a result text, an empty list) must
        // not erase the change the call already showed.
        if !has_diff(&content) && has_diff(&call.content) {
            let mut kept: Vec<acp::ToolCallContent> = call
                .content
                .drain(..)
                .filter(|content| matches!(content, acp::ToolCallContent::Diff(_)))
                .collect();
            kept.extend(content);
            call.content = kept;
        } else {
            call.content = content;
        }
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
    let from_input = call
        .raw_input
        .as_ref()
        .and_then(|input| match input.get("command") {
            Some(serde_json::Value::String(command)) => Some(command.clone()),
            Some(serde_json::Value::Array(parts)) => Some(
                parts
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
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
    let diffs: Vec<&acp::Diff> = call
        .content
        .iter()
        .filter_map(|content| match content {
            acp::ToolCallContent::Diff(diff) => Some(diff),
            _ => None,
        })
        .collect();
    if diffs.is_empty() {
        // No diff yet (or a delete/move without one): list the paths touched.
        return call
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
    let starts = hunk_starts(&diffs, &call.locations);
    // One change per path: an agent may send a diff block per hunk
    // (claude-agent-acp does once the edit is done), and the TUI keys
    // changes by path, so separate blocks would hide each other.
    let mut groups: Vec<(&Path, Vec<usize>)> = Vec::new();
    for (index, diff) in diffs.iter().enumerate() {
        match groups
            .iter_mut()
            .find(|(path, _)| *path == diff.path.as_path())
        {
            Some((_, members)) => members.push(index),
            None => groups.push((diff.path.as_path(), vec![index])),
        }
    }
    groups
        .into_iter()
        .map(|(path, members)| {
            let path = path.display().to_string();
            if let [only] = members.as_slice() {
                let diff = diffs[*only];
                if let Some(patch) = air_patch(diff) {
                    return patch.into_change(path);
                }
                if diff.old_text.is_none() {
                    // The TUI reads an added file's `diff` as its content.
                    return FileUpdateChange {
                        path,
                        kind: PatchChangeKind::Add,
                        diff: diff.new_text.clone(),
                    };
                }
            }
            let mut unified = String::new();
            // Lines before the next block, when no location places it.
            let (mut old_next, mut new_next) = (0_i64, 0_i64);
            // Lines the earlier blocks added, to place a block's old side.
            let mut delta = 0_i64;
            for index in members {
                let diff = diffs[index];
                if let Some(patch) = air_patch(diff) {
                    unified.push_str(&patch.hunks);
                    continue;
                }
                let old = diff.old_text.as_deref().unwrap_or("");
                let new = diff.new_text.as_str();
                let (old_skip, new_skip) = match &starts {
                    Some(starts) => {
                        let new_skip = i64::from(starts[index]) - 1;
                        (new_skip - delta, new_skip)
                    }
                    None => (old_next, new_next),
                };
                unified.push_str(&shift_hunks(
                    &fragment_diff(old, new, &path),
                    old_skip,
                    new_skip,
                ));
                let (old_lines, new_lines) = (line_count(old), line_count(new));
                delta += new_lines - old_lines;
                old_next = old_skip + old_lines;
                new_next = new_skip + new_lines;
            }
            FileUpdateChange {
                path,
                kind: PatchChangeKind::Update { move_path: None },
                diff: unified,
            }
        })
        .collect()
}

/// The first new line of each diff block, when every block has a location
/// that names it. claude-agent-acp reports a finished edit as one block per
/// hunk (the hunk's lines, context included) plus one location per hunk.
fn hunk_starts(diffs: &[&acp::Diff], locations: &[acp::ToolCallLocation]) -> Option<Vec<u32>> {
    if diffs.len() != locations.len() {
        return None;
    }
    diffs
        .iter()
        .zip(locations)
        .map(|(diff, location)| {
            (location.path == diff.path)
                .then_some(location.line)
                .flatten()
                .filter(|line| *line > 0)
        })
        .collect()
}

fn line_count(text: &str) -> i64 {
    i64::try_from(text.lines().count()).unwrap_or(i64::MAX)
}

/// The diff of a block that may be a fragment of the file (an edit's old and
/// new strings, or one hunk). Fragments end without a newline although the
/// file goes on; as-is, every last line would read "No newline at end of file".
fn fragment_diff(old: &str, new: &str, path: &str) -> String {
    if old.ends_with('\n') || new.ends_with('\n') {
        return unified_diff(old, new, path);
    }
    let terminate = |text: &str| {
        if text.is_empty() {
            String::new()
        } else {
            format!("{text}\n")
        }
    };
    unified_diff(&terminate(old), &terminate(new), path)
}

/// Move every hunk of `diff` down by the given number of lines.
fn shift_hunks(diff: &str, old_skip: i64, new_skip: i64) -> String {
    if old_skip == 0 && new_skip == 0 {
        return diff.to_string();
    }
    let shift = |range: &str, skip: i64| -> Option<String> {
        let (start, count) = match range.split_once(',') {
            Some((start, count)) => (start, Some(count)),
            None => (range, None),
        };
        let start = (start.parse::<i64>().ok()? + skip).max(0);
        Some(match count {
            Some(count) => format!("{start},{count}"),
            None => start.to_string(),
        })
    };
    diff.lines()
        .map(|line| {
            let shifted = line.strip_prefix("@@ -").and_then(|rest| {
                let (ranges, tail) = rest.split_once(" @@")?;
                let (old, new) = ranges.split_once(" +")?;
                Some(format!(
                    "@@ -{} +{} @@{tail}",
                    shift(old, old_skip)?,
                    shift(new, new_skip)?
                ))
            });
            format!("{}\n", shifted.unwrap_or_else(|| line.to_string()))
        })
        .collect()
}

/// A git patch carried in the JetBrains AIR form of a diff block: `oldText:
/// null`, `newText: ""`, and the patch under `_meta.jetbrains.air.diffPatch`.
/// acpc does not advertise `diffPatch`, so agents should not send it; read it
/// anyway rather than show an empty added file.
struct AirPatch {
    created: bool,
    hunks: String,
}

impl AirPatch {
    fn into_change(self, path: String) -> FileUpdateChange {
        if self.created {
            let content = self
                .hunks
                .lines()
                .filter_map(|line| line.strip_prefix('+'))
                .map(|line| format!("{line}\n"))
                .collect();
            return FileUpdateChange {
                path,
                kind: PatchChangeKind::Add,
                diff: content,
            };
        }
        FileUpdateChange {
            path,
            kind: PatchChangeKind::Update { move_path: None },
            diff: self.hunks,
        }
    }
}

fn air_patch(diff: &acp::Diff) -> Option<AirPatch> {
    let patch = diff
        .meta
        .as_ref()?
        .get("jetbrains")?
        .get("air")?
        .get("diffPatch")?;
    if patch.get("format")?.as_str()? != "git_patch" {
        return None;
    }
    let text = patch.get("text")?.as_str()?;
    let created = text
        .lines()
        .take_while(|line| !line.starts_with("@@"))
        .any(|line| line == "--- /dev/null");
    let hunks = text
        .lines()
        .skip_while(|line| !line.starts_with("@@"))
        .map(|line| format!("{line}\n"))
        .collect();
    Some(AirPatch { created, hunks })
}

const MAX_RESOLVED_FILE_BYTES: u64 = 1024 * 1024;

/// Turn the optimistic diffs of a call that has not run yet into diffs of
/// the whole file, by reading the file as it is now. Agents build these
/// diffs from the tool input alone: claude-agent-acp sends a Write as the
/// creation of its text (`oldText: null`) even when the file exists, and an
/// Edit as its old and new strings, without line numbers. Only call this
/// before the tool runs, such as when the agent asks permission for it.
pub(crate) fn resolve_against_files(call: &mut acp::ToolCall, cwd: &Path) {
    let replace_all = call
        .raw_input
        .as_ref()
        .and_then(|input| input.get("replace_all"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    for content in &mut call.content {
        let acp::ToolCallContent::Diff(diff) = content else {
            continue;
        };
        if air_patch(diff).is_some() {
            continue;
        }
        let path = if diff.path.is_absolute() {
            diff.path.clone()
        } else {
            cwd.join(&diff.path)
        };
        let Some(text) = std::fs::metadata(&path)
            .ok()
            .filter(|meta| meta.is_file() && meta.len() <= MAX_RESOLVED_FILE_BYTES)
            .and_then(|_| std::fs::read_to_string(&path).ok())
        else {
            continue;
        };
        match diff.old_text.as_deref() {
            None if text != diff.new_text => diff.old_text = Some(text),
            Some(old) if !old.is_empty() && old != text => {
                let found = text.matches(old).count();
                if found == 1 || (replace_all && found > 0) {
                    diff.new_text = if replace_all {
                        text.replace(old, &diff.new_text)
                    } else {
                        text.replacen(old, &diff.new_text, 1)
                    };
                    diff.old_text = Some(text);
                }
            }
            _ => {}
        }
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
        let other_with_diff =
            acp::ToolCall::new("4", "x").content(vec![acp::ToolCallContent::Diff(acp::Diff::new(
                "/a.rs", "new",
            ))]);
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

    fn diff(path: &str, old: Option<&str>, new: &str) -> acp::ToolCallContent {
        let diff = acp::Diff::new(path, new);
        acp::ToolCallContent::Diff(match old {
            Some(old) => diff.old_text(old),
            None => diff,
        })
    }

    fn edit(content: Vec<acp::ToolCallContent>) -> acp::ToolCall {
        acp::ToolCall::new("1", "Edit")
            .kind(acp::ToolKind::Edit)
            .content(content)
    }

    /// The `+N -M` the TUI draws: it parses the diff with `diffy`, and counts
    /// an added file's lines.
    fn counts(change: &FileUpdateChange) -> (usize, usize) {
        match change.kind {
            PatchChangeKind::Add => (change.diff.lines().count(), 0),
            PatchChangeKind::Delete => (0, change.diff.lines().count()),
            PatchChangeKind::Update { .. } => {
                let patch = diffy::Patch::from_str(&change.diff).expect("diffy parses the diff");
                patch.hunks().iter().flat_map(diffy::Hunk::lines).fold(
                    (0, 0),
                    |(added, removed), line| match line {
                        diffy::Line::Insert(_) => (added + 1, removed),
                        diffy::Line::Delete(_) => (added, removed + 1),
                        diffy::Line::Context(_) => (added, removed),
                    },
                )
            }
        }
    }

    #[test]
    fn created_file_carries_its_content() {
        let changes = file_changes(&edit(vec![diff("/n.txt", None, "one\ntwo\n")]));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, PatchChangeKind::Add);
        assert_eq!(changes[0].diff, "one\ntwo\n");
        assert_eq!(counts(&changes[0]), (2, 0));
    }

    #[test]
    fn hunks_of_one_file_merge_at_their_lines() {
        // claude-agent-acp's final diff: a block and a located line per hunk.
        let call = edit(vec![
            diff("/f.txt", Some("a\nb\nc"), "a\nB\nB2\nc"),
            diff("/f.txt", Some("x\ny\nz"), "x\nY\nz"),
        ])
        .locations(vec![
            acp::ToolCallLocation::new("/f.txt").line(4),
            acp::ToolCallLocation::new("/f.txt").line(21),
        ]);
        let changes = file_changes(&call);
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].diff,
            "@@ -4,3 +4,4 @@\n a\n-b\n+B\n+B2\n c\n@@ -20,3 +21,3 @@\n x\n-y\n+Y\n z\n"
        );
        assert_eq!(counts(&changes[0]), (3, 2));
    }

    #[test]
    fn unlocated_blocks_of_one_file_stack() {
        let call = edit(vec![
            diff("/f.txt", Some("b"), "B"),
            diff("/f.txt", Some("y"), "Y"),
        ]);
        let changes = file_changes(&call);
        assert_eq!(changes.len(), 1);
        assert_eq!(counts(&changes[0]), (2, 2));
    }

    #[test]
    fn edit_strings_diff_without_newline_markers() {
        let changes = file_changes(&edit(vec![diff("/f.txt", Some("beta"), "BETA")]));
        assert_eq!(changes[0].diff, "@@ -1 +1 @@\n-beta\n+BETA\n");
    }

    #[test]
    fn air_patch_form_reads_the_patch() {
        let mut block = acp::Diff::new("/f.txt", "");
        block.meta = serde_json::from_value(serde_json::json!({
            "jetbrains": {"air": {"diffPatch": {
                "version": 1,
                "format": "git_patch",
                "text": "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -2 +2 @@\n-b\n+B\n",
            }}}
        }))
        .expect("meta");
        let changes = file_changes(&edit(vec![acp::ToolCallContent::Diff(block)]));
        assert_eq!(changes[0].kind, PatchChangeKind::Update { move_path: None });
        assert_eq!(changes[0].diff, "@@ -2 +2 @@\n-b\n+B\n");
    }

    #[test]
    fn diffless_update_keeps_the_diff() {
        let mut call = edit(vec![diff("/f.txt", Some("b"), "B")]);
        apply_update(&mut call, acp::ToolCallUpdateFields::new().content(vec![]));
        assert!(has_diff(&call.content));
        apply_update(
            &mut call,
            acp::ToolCallUpdateFields::new().content(vec!["failed".into()]),
        );
        assert!(has_diff(&call.content));
        assert_eq!(output_text(&call), "failed");
    }

    #[test]
    fn pending_diffs_resolve_against_the_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let existing = dir.path().join("old.txt");
        std::fs::write(&existing, "a\nb\nc\nd\ne\n").expect("write");
        let edited = dir.path().join("edit.txt");
        std::fs::write(&edited, "alpha\nbeta\ngamma\n").expect("write");
        let created = dir.path().join("new.txt");
        let path = |p: &std::path::Path| p.to_str().expect("utf-8").to_string();
        let mut call = edit(vec![
            diff(&path(&existing), None, "a\nb\nX\nd\ne\n"),
            diff(&path(&edited), Some("beta"), "BETA"),
            diff(&path(&created), None, "one\n"),
        ]);
        resolve_against_files(&mut call, dir.path());
        let changes = file_changes(&call);
        assert_eq!(changes[0].kind, PatchChangeKind::Update { move_path: None });
        assert_eq!(counts(&changes[0]), (1, 1));
        assert_eq!(
            changes[1].diff,
            "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
        );
        assert_eq!(changes[2].kind, PatchChangeKind::Add);
    }

    #[test]
    fn unified_diff_omits_file_headers() {
        let diff = unified_diff("a\nb\n", "a\nc\n", "/f.txt");
        assert_eq!(diff, "@@ -1,2 +1,2 @@\n a\n-b\n+c\n");
    }
}
