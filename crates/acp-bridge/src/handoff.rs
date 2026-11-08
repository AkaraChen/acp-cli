//! Carrying a conversation to another agent (`/agent`, "Bring this
//! conversation"). ACP has no way to import history, so the transcript goes
//! with the new thread's first prompt: as an embedded resource with an
//! `acpc://handoff/…` URI when the agent accepts embedded context, else as a
//! text block between markers. The user never sees it: live turns record
//! only what the user typed, and replays drop it (`is_handoff`, `strip`).

use acp_client::schema as acp;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::UserInput;

use crate::server::Bridge;
use crate::threads::ThreadState;

const URI_PREFIX: &str = "acpc://handoff/";
const TEXT_OPEN: &str = "<acpc-handoff>";
const TEXT_CLOSE: &str = "</acpc-handoff>";
/// Transcript budget in characters; the oldest exchanges are dropped first.
const MAX_CHARS: usize = 60_000;

/// A conversation waiting to go out with a thread's first prompt.
#[derive(Debug, Clone)]
pub(crate) struct Handoff {
    pub(crate) from_name: String,
    uri: String,
    text: String,
}

/// The thread just created on `agent` carries over the conversation the
/// user asked for when switching to it, if any.
pub(crate) fn attach(bridge: &mut Bridge, agent: &str, thread_id: &str) {
    let Some((target, from)) = bridge.agents.handoff.take() else {
        return;
    };
    if target != agent {
        return;
    }
    let Some(handoff) = bridge.threads.get(&from).and_then(build) else {
        return;
    };
    if let Some(thread) = bridge.threads.get_mut(thread_id) {
        thread.handoff = Some(handoff);
    }
}

fn build(from: &ThreadState) -> Option<Handoff> {
    let mut exchanges: Vec<String> = from
        .turns
        .iter()
        .map(|turn| {
            turn.items
                .iter()
                .filter_map(|item| render(&item.payload, &from.agent_name))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|exchange| !exchange.is_empty())
        .collect();
    if exchanges.is_empty() {
        return None;
    }
    let mut size = 0;
    let keep = exchanges
        .iter()
        .rev()
        .take_while(|exchange| {
            size += exchange.len();
            size <= MAX_CHARS
        })
        .count()
        .max(1);
    let omitted = exchanges.len() - keep;
    let mut transcript = exchanges.split_off(omitted).join("\n\n");
    if omitted > 0 {
        transcript = format!("({omitted} earlier exchanges omitted)\n\n{transcript}");
    }
    let session = from.session_id.as_ref().map(ToString::to_string).unwrap_or_default();
    let text = format!(
        "The user was working with {name} in acpc and switched to you. Their conversation so far \
         follows, for context; continue from it. (They can go back to it with `acpc resume {id}`.)\n\n\
         {transcript}",
        name = from.agent_name,
        id = from.id,
    );
    Some(Handoff {
        from_name: from.agent_name.clone(),
        uri: format!("{URI_PREFIX}{}/{session}", from.agent),
        text,
    })
}

fn render(payload: &ThreadItemPayload, agent_name: &str) -> Option<String> {
    match payload {
        ThreadItemPayload::UserMessage { content, .. } => {
            let parts: Vec<String> = content
                .iter()
                .filter_map(|input| match input {
                    UserInput::Text { text, .. } => Some(text.clone()),
                    UserInput::Mention { path, .. } => Some(format!("@{path}")),
                    UserInput::LocalImage { path, .. } => Some(format!("[image {}]", path.display())),
                    UserInput::Image { .. } => Some("[image]".to_string()),
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| format!("User: {}", parts.join(" ")))
        }
        ThreadItemPayload::AgentMessage { text, .. } if !text.is_empty() => {
            Some(format!("{agent_name}: {text}"))
        }
        ThreadItemPayload::CommandExecution { command, .. } => Some(format!("({agent_name} ran `{command}`)")),
        ThreadItemPayload::FileChange { changes, .. } => {
            let paths: Vec<&str> = changes.iter().map(|change| change.path.as_str()).collect();
            Some(format!("({agent_name} edited {})", paths.join(", ")))
        }
        ThreadItemPayload::DynamicToolCall { tool, .. } => Some(format!("({agent_name}: {tool})")),
        _ => None,
    }
}

/// The content block that carries `handoff` to the new agent.
pub(crate) fn block(handoff: &Handoff, embedded_context: bool) -> acp::ContentBlock {
    if embedded_context {
        let contents = acp::TextResourceContents::new(handoff.text.clone(), handoff.uri.clone())
            .mime_type(Some("text/markdown".to_string()));
        return acp::ContentBlock::Resource(acp::EmbeddedResource::new(
            acp::EmbeddedResourceResource::TextResourceContents(contents),
        ));
    }
    acp::ContentBlock::Text(acp::TextContent::new(format!("{TEXT_OPEN}\n{}\n{TEXT_CLOSE}", handoff.text)))
}

/// A replayed user block that is a handoff resource.
pub(crate) fn is_handoff(block: &acp::ContentBlock) -> bool {
    let acp::ContentBlock::Resource(resource) = block else {
        return false;
    };
    let uri = match &resource.resource {
        acp::EmbeddedResourceResource::TextResourceContents(text) => &text.uri,
        acp::EmbeddedResourceResource::BlobResourceContents(blob) => &blob.uri,
        _ => return false,
    };
    uri.starts_with(URI_PREFIX)
}

/// Replayed user text without a leading text-form handoff; `None` when
/// nothing else is left.
pub(crate) fn strip(text: &str) -> Option<&str> {
    let Some(rest) = text.strip_prefix(TEXT_OPEN) else {
        return Some(text);
    };
    let rest = rest.split_once(TEXT_CLOSE).map_or("", |(_, after)| after).trim_start();
    (!rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn handoff() -> Handoff {
        Handoff {
            from_name: "Claude".to_string(),
            uri: format!("{URI_PREFIX}claude/s1"),
            text: "User: hi\n\nClaude: hello".to_string(),
        }
    }

    #[test]
    fn resource_form_is_recognized_on_replay() {
        let block = block(&handoff(), true);
        assert!(is_handoff(&block));
        assert!(!is_handoff(&acp::ContentBlock::Text(acp::TextContent::new("hi"))));
    }

    #[test]
    fn text_form_is_stripped_on_replay() {
        let acp::ContentBlock::Text(text) = block(&handoff(), false) else {
            panic!("text block expected");
        };
        assert_eq!(strip(&text.text), None);
        assert_eq!(strip(&format!("{}\n\ncontinue please", text.text)), Some("continue please"));
        assert_eq!(strip("plain"), Some("plain"));
    }
}
