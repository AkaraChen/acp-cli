//! Forking a conversation into a new session (`thread/fork`; `/agent` uses
//! it to bring a conversation to another agent). ACP has no way to import
//! history, so the transcript goes with the fork's first prompt: as an
//! embedded resource with an `acpc://handoff/<agent>/<session>` URI when the
//! agent accepts embedded context, else as text between markers. The user
//! never sees it: live turns record only what the user typed, and replays
//! drop it, keeping only which session it came from (`parent`).

use acp_client::schema as acp;
use codex_app_server_protocol::ThreadItemPayload;
use codex_app_server_protocol::UserInput;

use crate::threads::ThreadState;
use crate::threads::thread_id_for;

const URI_PREFIX: &str = "acpc://handoff/";
const TEXT_OPEN: &str = "<acpc-handoff uri=\"";
const TEXT_CLOSE: &str = "</acpc-handoff>";
/// Transcript budget in characters; the oldest exchanges are dropped first.
const MAX_CHARS: usize = 60_000;

/// A conversation waiting to go out with a thread's first prompt.
#[derive(Debug, Clone)]
pub(crate) struct Handoff {
    uri: String,
    text: String,
}

/// The session a fork started from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parent {
    pub(crate) agent: String,
    pub(crate) session_id: String,
}

impl Parent {
    pub(crate) fn of(thread: &ThreadState) -> Option<Self> {
        Some(Self {
            agent: thread.agent.clone(),
            session_id: thread.session_id.as_ref()?.to_string(),
        })
    }

    pub(crate) fn thread_id(&self) -> String {
        thread_id_for(&self.agent, &self.session_id)
    }

    fn uri(&self) -> String {
        format!("{URI_PREFIX}{}/{}", self.agent, self.session_id)
    }

    fn from_uri(uri: &str) -> Option<Self> {
        let (agent, session_id) = uri.strip_prefix(URI_PREFIX)?.split_once('/')?;
        Some(Self {
            agent: agent.to_string(),
            session_id: session_id.to_string(),
        })
    }
}

/// The transcript of `from` for its fork's first prompt.
pub(crate) fn build(from: &ThreadState) -> Option<Handoff> {
    let parent = Parent::of(from)?;
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
    let text = format!(
        "The user was working with {name} in acpc and switched to you. Their conversation so far \
         follows, for context; continue from it. (They can go back to it with `acpc resume -a {agent} {id}`.)\n\n\
         {transcript}",
        name = from.agent_name,
        agent = from.agent,
        id = from.id,
    );
    Some(Handoff {
        uri: parent.uri(),
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
                    UserInput::LocalImage { path, .. } => {
                        Some(format!("[image {}]", path.display()))
                    }
                    UserInput::Image { .. } => Some("[image]".to_string()),
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| format!("User: {}", parts.join(" ")))
        }
        ThreadItemPayload::AgentMessage { text, .. } if !text.is_empty() => {
            Some(format!("{agent_name}: {text}"))
        }
        ThreadItemPayload::CommandExecution { command, .. } => {
            Some(format!("({agent_name} ran `{command}`)"))
        }
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
    acp::ContentBlock::Text(acp::TextContent::new(format!(
        "{TEXT_OPEN}{}\">\n{}\n{TEXT_CLOSE}",
        handoff.uri, handoff.text
    )))
}

/// The session a replayed user block says this one was forked from.
pub(crate) fn parent(block: &acp::ContentBlock) -> Option<Parent> {
    match block {
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => {
                Parent::from_uri(&text.uri)
            }
            acp::EmbeddedResourceResource::BlobResourceContents(blob) => {
                Parent::from_uri(&blob.uri)
            }
            _ => None,
        },
        acp::ContentBlock::Text(text) => {
            let rest = text.text.strip_prefix(TEXT_OPEN)?;
            Parent::from_uri(rest.split_once('"')?.0)
        }
        _ => None,
    }
}

/// A replayed user block that is a handoff resource.
pub(crate) fn is_handoff(block: &acp::ContentBlock) -> bool {
    matches!(block, acp::ContentBlock::Resource(_)) && parent(block).is_some()
}

/// Replayed user text without a leading text-form handoff; `None` when
/// nothing else is left.
pub(crate) fn strip(text: &str) -> Option<&str> {
    let Some(rest) = text.strip_prefix(TEXT_OPEN) else {
        return Some(text);
    };
    let rest = rest
        .split_once(TEXT_CLOSE)
        .map_or("", |(_, after)| after)
        .trim_start();
    (!rest.is_empty()).then_some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn handoff() -> Handoff {
        Handoff {
            uri: format!("{URI_PREFIX}claude/s1"),
            text: "User: hi\n\nClaude: hello".to_string(),
        }
    }

    fn claude_s1() -> Option<Parent> {
        Some(Parent {
            agent: "claude".to_string(),
            session_id: "s1".to_string(),
        })
    }

    #[test]
    fn resource_form_is_recognized_on_replay() {
        let block = block(&handoff(), true);
        assert!(is_handoff(&block));
        assert_eq!(parent(&block), claude_s1());
        assert!(!is_handoff(&acp::ContentBlock::Text(
            acp::TextContent::new("hi")
        )));
    }

    #[test]
    fn text_form_is_stripped_on_replay() {
        let block = block(&handoff(), false);
        assert_eq!(parent(&block), claude_s1());
        let acp::ContentBlock::Text(text) = block else {
            panic!("text block expected");
        };
        assert_eq!(strip(&text.text), None);
        assert_eq!(
            strip(&format!("{}\n\ncontinue please", text.text)),
            Some("continue please")
        );
        assert_eq!(strip("plain"), Some("plain"));
    }
}
