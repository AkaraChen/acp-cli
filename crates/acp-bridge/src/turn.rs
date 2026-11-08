//! Reduces one ACP prompt turn into a Codex `Turn` and its live notifications.
//!
//! The same reducer serves live turns and `session/load` replay; replay
//! simply discards the notifications and keeps the items.

use std::collections::HashMap;
use std::path::PathBuf;

use acp_client::schema as acp;
use codex_app_server_protocol::AgentMessageDeltaNotification;
use codex_app_server_protocol::CommandExecutionOutputDeltaNotification;
use codex_app_server_protocol::FileChangePatchUpdatedNotification;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ReasoningSummaryTextDeltaNotification;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnError;
use codex_app_server_protocol::TurnPlanStep;
use codex_app_server_protocol::TurnPlanStepStatus;
use codex_app_server_protocol::TurnPlanUpdatedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;

use crate::content::display_text;
use crate::records;
use crate::tool_calls;
use crate::tool_calls::ItemKind;
use crate::tool_calls::Outcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Message,
    Reasoning,
}

struct OpenText {
    kind: TextKind,
    index: usize,
    message_id: Option<String>,
    text: String,
}

struct ToolEntry {
    index: usize,
    kind: ItemKind,
    call: acp::ToolCall,
    started_at_ms: i64,
    emitted_output: String,
    finished: bool,
}

pub(crate) struct TurnBuilder {
    pub(crate) id: String,
    thread_id: String,
    cwd: PathBuf,
    pub(crate) started_at_ms: i64,
    items: Vec<ThreadItem>,
    open_text: Option<OpenText>,
    tools: HashMap<String, ToolEntry>,
    next_item: u64,
}

pub(crate) type Emit = Vec<ServerNotification>;

impl TurnBuilder {
    pub(crate) fn new(id: String, thread_id: String, cwd: PathBuf, started_at_ms: i64) -> Self {
        Self {
            id,
            thread_id,
            cwd,
            started_at_ms,
            items: Vec::new(),
            open_text: None,
            tools: HashMap::new(),
            next_item: 1,
        }
    }

    fn next_item_id(&mut self) -> String {
        let id = format!("{}-{}", self.id, self.next_item);
        self.next_item += 1;
        id
    }

    pub(crate) fn has_items(&self) -> bool {
        !self.items.is_empty()
    }

    fn started(&self, index: usize, emit: &mut Emit) {
        emit.push(ServerNotification::ItemStarted(ItemStartedNotification {
            item: self.items[index].clone(),
            thread_id: self.thread_id.clone(),
            turn_id: self.id.clone(),
            started_at_ms: self.items[index]
                .started_at_ms
                .unwrap_or(self.started_at_ms),
        }));
    }

    fn completed(&mut self, index: usize, emit: &mut Emit) {
        let now = records::now_ms();
        self.items[index].completed_at_ms = Some(now);
        emit.push(ServerNotification::ItemCompleted(ItemCompletedNotification {
            item: self.items[index].clone(),
            thread_id: self.thread_id.clone(),
            turn_id: self.id.clone(),
            content_token_count: None,
            completed_at_ms: now,
        }));
    }

    /// A user prompt replayed by `session/load`.
    pub(crate) fn user_chunk(&mut self, chunk: &acp::ContentChunk, emit: &mut Emit) {
        self.close_text(emit);
        let Some(input) = crate::content::user_input(&chunk.content) else {
            return;
        };
        // Consecutive chunks of one user message extend the same item.
        if let Some(ThreadItem {
            payload: ThreadItemPayload::UserMessage { content, .. },
            ..
        }) = self.items.last_mut()
        {
            match (content.last_mut(), &input) {
                (
                    Some(UserInput::Text { text, .. }),
                    UserInput::Text {
                        text: more,
                        ..
                    },
                ) => text.push_str(more),
                _ => content.push(input),
            }
            return;
        }
        let id = self.next_item_id();
        let now = records::now_ms();
        let mut item = records::item(
            ThreadItemPayload::UserMessage {
                id,
                client_id: None,
                content: vec![input],
            },
            now,
        );
        item.completed_at_ms = Some(now);
        self.items.push(item);
    }

    pub(crate) fn has_agent_output(&self) -> bool {
        self.items
            .iter()
            .any(|item| !matches!(item.payload, ThreadItemPayload::UserMessage { .. }))
    }

    pub(crate) fn agent_message(&mut self, chunk: &acp::ContentChunk, emit: &mut Emit) {
        self.text(TextKind::Message, chunk, emit);
    }

    pub(crate) fn agent_thought(&mut self, chunk: &acp::ContentChunk, emit: &mut Emit) {
        self.text(TextKind::Reasoning, chunk, emit);
    }

    fn text(&mut self, kind: TextKind, chunk: &acp::ContentChunk, emit: &mut Emit) {
        let delta = display_text(&chunk.content);
        let message_id = chunk.message_id.as_ref().map(ToString::to_string);
        let continues = self.open_text.as_ref().is_some_and(|open| {
            open.kind == kind
                && (message_id.is_none() || open.message_id.is_none() || open.message_id == message_id)
        });
        if !continues {
            self.close_text(emit);
            let id = self.next_item_id();
            let payload = match kind {
                TextKind::Message => ThreadItemPayload::AgentMessage {
                    moderation: None,
                    id,
                    text: String::new(),
                    phase: None,
                    memory_citation: None,
                    delivery: None,
                    questions: None,
                },
                TextKind::Reasoning => ThreadItemPayload::Reasoning {
                    id,
                    summary: Vec::new(),
                    content: Vec::new(),
                },
            };
            self.items.push(records::item(payload, records::now_ms()));
            let index = self.items.len() - 1;
            self.started(index, emit);
            self.open_text = Some(OpenText {
                kind,
                index,
                message_id: message_id.clone(),
                text: String::new(),
            });
        }
        let Some(open) = self.open_text.as_mut() else {
            return;
        };
        if open.message_id.is_none() {
            open.message_id = message_id;
        }
        if delta.is_empty() {
            return;
        }
        open.text.push_str(&delta);
        let item_id = item_id(&self.items[open.index].payload).to_string();
        match open.kind {
            TextKind::Message => emit.push(ServerNotification::AgentMessageDelta(
                AgentMessageDeltaNotification {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.id.clone(),
                    item_id,
                    delta,
                },
            )),
            TextKind::Reasoning => emit.push(ServerNotification::ReasoningSummaryTextDelta(
                ReasoningSummaryTextDeltaNotification {
                    thread_id: self.thread_id.clone(),
                    turn_id: self.id.clone(),
                    item_id,
                    delta,
                    summary_index: 0,
                },
            )),
        }
    }

    fn close_text(&mut self, emit: &mut Emit) {
        let Some(open) = self.open_text.take() else {
            return;
        };
        match &mut self.items[open.index].payload {
            ThreadItemPayload::AgentMessage { text, .. } => *text = open.text,
            ThreadItemPayload::Reasoning { summary, .. } if !open.text.is_empty() => {
                *summary = vec![open.text];
            }
            _ => {}
        }
        self.completed(open.index, emit);
    }

    /// The item id assigned to an ACP tool call, creating the item if this is
    /// the first time the call is seen.
    pub(crate) fn tool_item(
        &mut self,
        update: &acp::ToolCallUpdate,
        emit: &mut Emit,
    ) -> (String, ItemKind) {
        let key = update.tool_call_id.to_string();
        if !self.tools.contains_key(&key) {
            self.tool_call(tool_calls::from_update(update), emit);
        } else {
            self.tool_call_update(update.clone(), emit);
        }
        let entry = &self.tools[&key];
        (
            item_id(&self.items[entry.index].payload).to_string(),
            entry.kind,
        )
    }

    pub(crate) fn tool_call(&mut self, call: acp::ToolCall, emit: &mut Emit) {
        let key = call.tool_call_id.to_string();
        if self.tools.contains_key(&key) {
            // Agents may re-announce a call; treat it as a full update.
            let fields = acp::ToolCallUpdateFields::new()
                .kind(call.kind)
                .status(call.status)
                .title(call.title.clone())
                .content(call.content.clone())
                .locations(call.locations.clone())
                .raw_input(call.raw_input.clone())
                .raw_output(call.raw_output.clone());
            self.tool_call_update(acp::ToolCallUpdate::new(call.tool_call_id, fields), emit);
            return;
        }
        self.close_text(emit);
        let kind = tool_calls::classify(&call);
        let id = self.next_item_id();
        let now = records::now_ms();
        let payload = tool_calls::payload(&id, kind, &call, Outcome::Running, &self.cwd, None);
        self.items.push(records::item(payload, now));
        let index = self.items.len() - 1;
        self.started(index, emit);
        let output = tool_calls::output_text(&call);
        let finished = tool_calls::outcome(&call) != Outcome::Running;
        self.tools.insert(
            key.clone(),
            ToolEntry {
                index,
                kind,
                call,
                started_at_ms: now,
                emitted_output: String::new(),
                finished: false,
            },
        );
        if kind == ItemKind::Command && !output.is_empty() {
            self.emit_output(&key, emit);
        }
        if finished {
            self.finish_tool(&key, None, emit);
        }
    }

    pub(crate) fn tool_call_update(&mut self, update: acp::ToolCallUpdate, emit: &mut Emit) {
        let key = update.tool_call_id.to_string();
        let Some(entry) = self.tools.get_mut(&key) else {
            // An update for an unannounced call: start it from what we know.
            let call = tool_calls::from_update(&update);
            self.tool_call(call, emit);
            return;
        };
        if entry.finished {
            return;
        }
        let previous_changes = match &self.items[entry.index].payload {
            ThreadItemPayload::FileChange { changes, .. } => Some(changes.clone()),
            _ => None,
        };
        tool_calls::apply_update(&mut entry.call, update.fields);
        let kind = entry.kind;
        let finished = tool_calls::outcome(&entry.call) != Outcome::Running;
        match kind {
            ItemKind::Command | ItemKind::Read | ItemKind::Dynamic => {
                // Keep the stored item current (an approval prompt shows it).
                let entry = &self.tools[&key];
                let index = entry.index;
                let item_id = item_id(&self.items[index].payload).to_string();
                self.items[index].payload = tool_calls::payload(
                    &item_id,
                    kind,
                    &entry.call,
                    Outcome::Running,
                    &self.cwd,
                    None,
                );
                if kind == ItemKind::Command {
                    self.emit_output(&key, emit);
                }
            }
            ItemKind::FileChange => {
                let entry = &self.tools[&key];
                let item_id = item_id(&self.items[entry.index].payload).to_string();
                let payload = tool_calls::payload(
                    &item_id,
                    kind,
                    &entry.call,
                    Outcome::Running,
                    &self.cwd,
                    None,
                );
                let index = entry.index;
                if let ThreadItemPayload::FileChange { changes, .. } = &payload
                    && previous_changes.as_ref() != Some(changes)
                {
                    emit.push(ServerNotification::FileChangePatchUpdated(
                        FileChangePatchUpdatedNotification {
                            thread_id: self.thread_id.clone(),
                            turn_id: self.id.clone(),
                            item_id,
                            changes: changes.clone(),
                        },
                    ));
                }
                self.items[index].payload = payload;
            }
        }
        if finished {
            self.finish_tool(&key, None, emit);
        }
    }

    fn emit_output(&mut self, key: &str, emit: &mut Emit) {
        let Some(entry) = self.tools.get_mut(key) else {
            return;
        };
        let output = tool_calls::output_text(&entry.call);
        let delta = match output.strip_prefix(entry.emitted_output.as_str()) {
            Some(delta) => delta.to_string(),
            // Output was rewritten; the completed item carries the full text.
            None => String::new(),
        };
        entry.emitted_output = output;
        if delta.is_empty() {
            return;
        }
        let item_id = item_id(&self.items[entry.index].payload).to_string();
        emit.push(ServerNotification::CommandExecutionOutputDelta(
            CommandExecutionOutputDeltaNotification {
                thread_id: self.thread_id.clone(),
                turn_id: self.id.clone(),
                item_id,
                delta,
            },
        ));
    }

    /// Complete a tool item. `forced` overrides the agent-reported outcome
    /// (used for declined permissions and turn cancellation).
    pub(crate) fn finish_tool(&mut self, key: &str, forced: Option<Outcome>, emit: &mut Emit) {
        let Some(entry) = self.tools.get_mut(key) else {
            return;
        };
        if entry.finished {
            return;
        }
        entry.finished = true;
        let outcome = forced.unwrap_or_else(|| match tool_calls::outcome(&entry.call) {
            Outcome::Running => Outcome::Failed,
            outcome => outcome,
        });
        let duration = records::now_ms().saturating_sub(entry.started_at_ms);
        let index = entry.index;
        let item_id = item_id(&self.items[index].payload).to_string();
        self.items[index].payload =
            tool_calls::payload(&item_id, entry.kind, &entry.call, outcome, &self.cwd, Some(duration));
        self.completed(index, emit);
    }

    /// The agent's current title for a tool call.
    pub(crate) fn tool_title(&self, tool_call_id: &acp::ToolCallId) -> Option<String> {
        self.tools
            .get(&tool_call_id.to_string())
            .map(|entry| entry.call.title.clone())
            .filter(|title| !title.is_empty())
    }

    pub(crate) fn item_payload(&self, wanted: &str) -> Option<ThreadItemPayload> {
        self.items
            .iter()
            .find(|item| item_id(&item.payload) == wanted)
            .map(|item| item.payload.clone())
    }

    pub(crate) fn plan(&self, plan: &acp::Plan, emit: &mut Emit) {
        emit.push(ServerNotification::TurnPlanUpdated(TurnPlanUpdatedNotification {
            thread_id: self.thread_id.clone(),
            turn_id: self.id.clone(),
            explanation: None,
            plan: plan
                .entries
                .iter()
                .map(|entry| TurnPlanStep {
                    step: entry.content.clone(),
                    status: match entry.status {
                        acp::PlanEntryStatus::Completed => TurnPlanStepStatus::Completed,
                        acp::PlanEntryStatus::InProgress => TurnPlanStepStatus::InProgress,
                        _ => TurnPlanStepStatus::Pending,
                    },
                })
                .collect(),
        }));
    }

    /// A context compaction reported by the agent (`compaction_update`).
    pub(crate) fn compaction(&mut self, update: &acp::CompactionUpdate, emit: &mut Emit) {
        let key = format!("compaction:{}", update.compaction_id);
        if let Some(entry) = self.tools.get(&key) {
            if !matches!(update.status, acp::CompactionStatus::InProgress) && !entry.finished {
                let index = entry.index;
                if let Some(entry) = self.tools.get_mut(&key) {
                    entry.finished = true;
                }
                self.completed(index, emit);
            }
            return;
        }
        self.close_text(emit);
        let id = self.next_item_id();
        self.items.push(records::item(
            ThreadItemPayload::ContextCompaction { id },
            records::now_ms(),
        ));
        let index = self.items.len() - 1;
        self.started(index, emit);
        let finished = !matches!(update.status, acp::CompactionStatus::InProgress);
        self.tools.insert(
            key,
            ToolEntry {
                index,
                kind: ItemKind::Dynamic,
                call: acp::ToolCall::new("compaction", "Compaction"),
                started_at_ms: records::now_ms(),
                emitted_output: String::new(),
                finished,
            },
        );
        if finished {
            self.completed(index, emit);
        }
    }

    /// The turn so far, for a client attaching while it runs.
    pub(crate) fn snapshot(&self) -> Turn {
        let mut items = self.items.clone();
        if let Some(open) = &self.open_text {
            match &mut items[open.index].payload {
                ThreadItemPayload::AgentMessage { text, .. } => text.clone_from(&open.text),
                ThreadItemPayload::Reasoning { summary, .. } => {
                    *summary = vec![open.text.clone()];
                }
                _ => {}
            }
        }
        records::turn(&self.id, items, TurnStatus::InProgress, None, Some(self.started_at_ms), None)
    }

    /// Close every open item and produce the finished turn.
    pub(crate) fn finish(
        mut self,
        status: TurnStatus,
        error: Option<TurnError>,
        emit: &mut Emit,
    ) -> Turn {
        self.close_text(emit);
        let cancelled = status == TurnStatus::Interrupted;
        let open: Vec<String> = self
            .tools
            .iter()
            .filter(|(key, entry)| !entry.finished && !key.starts_with("compaction:"))
            .map(|(key, _)| key.clone())
            .collect();
        for key in open {
            let forced = if cancelled {
                Outcome::Declined
            } else {
                Outcome::Failed
            };
            self.finish_tool(&key, Some(forced), emit);
        }
        let compactions: Vec<usize> = self
            .tools
            .iter()
            .filter(|(key, entry)| !entry.finished && key.starts_with("compaction:"))
            .map(|(_, entry)| entry.index)
            .collect();
        for index in compactions {
            self.completed(index, emit);
        }
        records::turn(
            &self.id,
            self.items,
            status,
            error,
            Some(self.started_at_ms),
            Some(records::now_ms()),
        )
    }
}

pub(crate) fn item_id(payload: &ThreadItemPayload) -> &str {
    match payload {
        ThreadItemPayload::UserMessage { id, .. }
        | ThreadItemPayload::AgentMessage { id, .. }
        | ThreadItemPayload::Reasoning { id, .. }
        | ThreadItemPayload::CommandExecution { id, .. }
        | ThreadItemPayload::FileChange { id, .. }
        | ThreadItemPayload::DynamicToolCall { id, .. }
        | ThreadItemPayload::ContextCompaction { id, .. } => id,
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn chunk(text: &str, message_id: Option<&str>) -> acp::ContentChunk {
        let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)));
        match message_id {
            Some(id) => chunk.message_id(acp::MessageId::new(id)),
            None => chunk,
        }
    }

    fn methods(emit: &Emit) -> Vec<String> {
        emit.iter()
            .map(|n| {
                serde_json::to_value(n)
                    .ok()
                    .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string))
                    .unwrap_or_default()
            })
            .collect()
    }

    fn builder() -> TurnBuilder {
        TurnBuilder::new("t1".into(), "th".into(), "/w".into(), 0)
    }

    #[test]
    fn message_chunks_stream_into_one_item_until_interrupted_by_a_tool() {
        let mut turn = builder();
        let mut emit = Vec::new();
        turn.agent_thought(&chunk("hmm", None), &mut emit);
        turn.agent_message(&chunk("Hel", None), &mut emit);
        turn.agent_message(&chunk("lo", None), &mut emit);
        turn.tool_call(
            acp::ToolCall::new("c1", "ls").kind(acp::ToolKind::Execute),
            &mut emit,
        );
        turn.agent_message(&chunk("done", None), &mut emit);
        let finished = turn.finish(TurnStatus::Completed, None, &mut emit);
        assert_eq!(
            methods(&emit),
            vec![
                "item/started",
                "item/reasoning/summaryTextDelta",
                "item/completed",
                "item/started",
                "item/agentMessage/delta",
                "item/agentMessage/delta",
                "item/completed",
                "item/started",
                "item/started",
                "item/agentMessage/delta",
                "item/completed",
                "item/completed",
            ]
        );
        let texts: Vec<String> = finished
            .items
            .iter()
            .filter_map(|item| match &item.payload {
                ThreadItemPayload::AgentMessage { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello".to_string(), "done".to_string()]);
    }

    #[test]
    fn changed_message_id_starts_a_new_message() {
        let mut turn = builder();
        let mut emit = Vec::new();
        turn.agent_message(&chunk("a", Some("m1")), &mut emit);
        turn.agent_message(&chunk("b", Some("m2")), &mut emit);
        let finished = turn.finish(TurnStatus::Completed, None, &mut emit);
        assert_eq!(finished.items.len(), 2);
    }

    #[test]
    fn cancelled_turn_declines_running_tools() {
        let mut turn = builder();
        let mut emit = Vec::new();
        turn.tool_call(
            acp::ToolCall::new("c1", "rm").kind(acp::ToolKind::Execute),
            &mut emit,
        );
        let finished = turn.finish(TurnStatus::Interrupted, None, &mut emit);
        assert!(matches!(
            finished.items[0].payload,
            ThreadItemPayload::CommandExecution {
                status: codex_app_server_protocol::CommandExecutionStatus::Declined,
                ..
            }
        ));
    }

    #[test]
    fn command_output_streams_as_deltas() {
        let mut turn = builder();
        let mut emit = Vec::new();
        turn.tool_call(
            acp::ToolCall::new("c1", "ls").kind(acp::ToolKind::Execute),
            &mut emit,
        );
        let text = |t: &str| {
            vec![acp::ToolCallContent::Content(acp::Content::new(
                acp::ContentBlock::Text(acp::TextContent::new(t)),
            ))]
        };
        turn.tool_call_update(
            acp::ToolCallUpdate::new("c1", acp::ToolCallUpdateFields::new().content(text("a\n"))),
            &mut emit,
        );
        turn.tool_call_update(
            acp::ToolCallUpdate::new(
                "c1",
                acp::ToolCallUpdateFields::new()
                    .content(text("a\nb\n"))
                    .status(acp::ToolCallStatus::Completed),
            ),
            &mut emit,
        );
        assert_eq!(
            methods(&emit),
            vec![
                "item/started",
                "item/commandExecution/outputDelta",
                "item/commandExecution/outputDelta",
                "item/completed",
            ]
        );
    }
}
