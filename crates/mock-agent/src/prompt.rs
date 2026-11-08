//! `session/prompt`: records the prompt, picks the scripted or demo steps,
//! and hands the turn to a spawned [`crate::turn`] runner.

use std::sync::Arc;

use agent_client_protocol::Error;
use agent_client_protocol::ErrorCode;
use agent_client_protocol::v1::Responder;
use agent_client_protocol::v1::schema as acp;

use crate::agent::Cx;
use crate::agent::MockAgent;
use crate::agent::Session;
use crate::agent::TurnHandle;
use crate::agent::now;
use crate::demo;
use crate::scenario::ACCEPT_EDITS_MODE;
use crate::scenario::AUTO_APPROVE_OPTION;
use crate::step::Step;
use crate::step::Vars;
use crate::turn;

/// Everything a turn needs, captured when `session/prompt` arrives.
pub struct TurnPlan {
    pub session_id: acp::SessionId,
    pub steps: Vec<Step>,
    /// Used when the steps run to completion.
    pub stop_reason: acp::StopReason,
    pub turn: TurnHandle,
    pub client: acp::ClientCapabilities,
    pub vars: Vars,
}

/// Handles `session/prompt`: validates and records the prompt, then runs the
/// turn on a spawned task so the dispatch loop stays free for
/// `session/cancel` and permission responses.
pub fn prompt(
    agent: &Arc<MockAgent>,
    request: acp::PromptRequest,
    responder: Responder<acp::PromptResponse>,
    cx: &Cx,
) -> Result<(), Error> {
    let plan = match start(agent, &request) {
        Ok(plan) => plan,
        Err(err) => return responder.respond_with_error(err),
    };
    agent.persist(&request.session_id.0);
    let agent = Arc::clone(agent);
    let task_cx = cx.clone();
    cx.spawn(async move {
        turn::run(&agent, &task_cx, plan, responder).await;
        Ok(())
    })
}

fn start(agent: &MockAgent, request: &acp::PromptRequest) -> Result<TurnPlan, Error> {
    agent.require_prompt_auth()?;
    let text = prompt_text(&request.prompt);
    let turn_id = agent.unique_id("call");
    agent.with_active(&request.session_id, |client, session| {
        if session.turn.is_some() {
            return Err(Error::new(
                ErrorCode::INVALID_REQUEST,
                "a prompt turn is already running for this session",
            ));
        }
        for block in &request.prompt {
            let chunk = acp::ContentChunk::new(block.clone());
            session
                .record
                .history
                .push(acp::SessionUpdate::UserMessageChunk(chunk));
        }
        let new_title = session.record.title.is_none().then(|| title_for(&text));
        if new_title.is_some() {
            session.record.title.clone_from(&new_title);
        }
        session.record.updated_at = Some(now());
        session.persist = true;
        session.used_tokens += 1_000 + 4 * text.len() as u64;

        let vars: Vars = [
            ("prompt", text.clone()),
            ("session_id", request.session_id.to_string()),
            ("cwd", session.record.cwd.display().to_string()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
        let (steps, stop_reason) = match agent.scenario.find_turn(&text) {
            Some(turn) => (turn.steps.clone(), turn.stop_reason),
            None => demo::plan(&demo::DemoInput {
                prompt: &text,
                session_id: &request.session_id,
                cwd: &session.record.cwd,
                turn_id: &turn_id,
                new_title: new_title.as_deref(),
                form_elicitation: client
                    .elicitation
                    .as_ref()
                    .is_some_and(acp::ElicitationCapabilities::supports_form),
                auto_approve: auto_approve(session),
                used_tokens: session.used_tokens,
            }),
        };
        let turn = TurnHandle::new();
        session.turn = Some(turn.clone());
        Ok(TurnPlan {
            session_id: request.session_id.clone(),
            steps,
            stop_reason,
            turn,
            client: client.clone(),
            vars,
        })
    })
}

/// The demo skips edit permission when `auto_approve` is on or the session
/// is in the `accept_edits` mode.
fn auto_approve(session: &Session) -> bool {
    let option_on = session.config_options.iter().any(|option| {
        option.id.0.as_ref() == AUTO_APPROVE_OPTION
            && matches!(&option.kind, acp::SessionConfigKind::Boolean(b) if b.current_value)
    });
    let mode_on = session
        .modes
        .as_ref()
        .is_some_and(|modes| modes.current_mode_id.0.as_ref() == ACCEPT_EDITS_MODE);
    option_on || mode_on
}

/// The prompt's text blocks, joined by newlines.
pub fn prompt_text(prompt: &[acp::ContentBlock]) -> String {
    prompt
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn title_for(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let title: String = line.trim().chars().take(48).collect();
    if title.is_empty() {
        "Untitled session".to_string()
    } else {
        title
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(title_for("\n  fix the bug \nmore"), "fix the bug");
        assert_eq!(title_for(""), "Untitled session");
    }

    #[test]
    fn prompt_text_joins_text_blocks() {
        let prompt = vec!["a".into(), "b".into()];
        assert_eq!(prompt_text(&prompt), "a\nb");
    }
}
