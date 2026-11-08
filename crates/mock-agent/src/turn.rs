//! Executes a turn's [`Step`]s on a spawned task and answers
//! `session/prompt` with the resulting stop reason (or error).

use std::time::Duration;

use agent_client_protocol::BoxFuture;
use agent_client_protocol::Error;
use agent_client_protocol::ErrorCode;
use agent_client_protocol::JsonRpcRequest;
use agent_client_protocol::v1::Responder;
use agent_client_protocol::v1::schema as acp;
use tokio_util::sync::CancellationToken;

use crate::agent::Cx;
use crate::agent::MockAgent;
use crate::prompt::TurnPlan;
use crate::step::ElicitationStep;
use crate::step::Payload;
use crate::step::PermissionStep;
use crate::step::Step;
use crate::step::Vars;

/// Pause between streamed text chunks.
const CHUNK_DELAY: Duration = Duration::from_millis(15);
/// How long to wait for the client to answer a pending request after the
/// turn was cancelled (it must answer with `cancelled`).
const CANCEL_GRACE: Duration = Duration::from_secs(2);

pub async fn run(
    agent: &MockAgent,
    cx: &Cx,
    plan: TurnPlan,
    responder: Responder<acp::PromptResponse>,
) {
    let TurnPlan {
        session_id,
        steps,
        stop_reason,
        turn,
        client,
        vars,
    } = plan;
    // Close and delete answer once this fires: after the prompt response,
    // so the client sees nothing for the session after their response.
    let done = turn.done.drop_guard();
    let cancel = turn.cancel;
    let mut runner = Runner {
        agent,
        cx,
        session_id: session_id.clone(),
        cancel: cancel.clone(),
        client: &client,
        vars,
        cleanup: false,
    };
    // `$/cancel_request` on the prompt request acts like `session/cancel`.
    let request_cancel = responder.cancellation();
    let watch_request_cancel = async {
        request_cancel.cancelled().await;
        cancel.cancel();
        std::future::pending::<()>().await;
    };
    let flow = tokio::select! {
        flow = runner.run(&steps) => flow,
        () = watch_request_cancel => Flow::Continue,
    };
    // The spec requires `cancelled` once the client cancelled, whatever happened.
    let result = match flow {
        _ if cancel.is_cancelled() => Ok(acp::StopReason::Cancelled),
        Flow::Continue => Ok(stop_reason),
        Flow::Stop(reason) => Ok(reason),
        Flow::Fail(err) => Err(err),
    };
    agent.finish_turn(&session_id);
    if let Err(err) = responder.respond_with_result(result.map(acp::PromptResponse::new)) {
        log!("failed to answer session/prompt: {err}");
    }
    drop(done);
}

enum Flow {
    Continue,
    Stop(acp::StopReason),
    Fail(Error),
}

struct Runner<'a> {
    agent: &'a MockAgent,
    cx: &'a Cx,
    session_id: acp::SessionId,
    cancel: CancellationToken,
    client: &'a acp::ClientCapabilities,
    vars: Vars,
    /// Running a `cancelled`/`cancel` branch after the turn was cancelled:
    /// its steps run to completion and sleeps are not interrupted.
    cleanup: bool,
}

impl Runner<'_> {
    fn run<'s>(&'s mut self, steps: &'s [Step]) -> BoxFuture<'s, Flow> {
        Box::pin(async move {
            for step in steps {
                if self.cancel.is_cancelled() && !self.cleanup {
                    return Flow::Stop(acp::StopReason::Cancelled);
                }
                match self.step(step).await {
                    Flow::Continue => {}
                    other => return other,
                }
            }
            Flow::Continue
        })
    }

    /// Runs a branch; a cancellation branch taken because the turn was
    /// cancelled still runs in full (e.g. to mark a tool call failed).
    async fn run_branch(&mut self, steps: &[Step], cancellation_branch: bool) -> Flow {
        let cleanup = self.cleanup;
        self.cleanup = cleanup || (cancellation_branch && self.cancel.is_cancelled());
        let flow = self.run(steps).await;
        self.cleanup = cleanup;
        flow
    }

    async fn step(&mut self, step: &Step) -> Flow {
        match step {
            Step::Update(update) => match self.render(update) {
                // A raw notice still honours the client's `session.notices`.
                Ok(acp::SessionUpdate::Notice(notice)) => self.notice(&notice),
                Ok(update) => self.send(update),
                Err(flow) => flow,
            },
            Step::Sleep(duration) => self.sleep(*duration).await,
            Step::Text(text) => {
                self.stream(text, acp::SessionUpdate::AgentMessageChunk)
                    .await
            }
            Step::Thought(text) => {
                self.stream(text, acp::SessionUpdate::AgentThoughtChunk)
                    .await
            }
            Step::Permission(permission) => self.permission(permission).await,
            Step::Elicitation(elicitation) => self.elicitation(elicitation).await,
            Step::WaitForCancel => {
                self.cancel.cancelled().await;
                Flow::Stop(acp::StopReason::Cancelled)
            }
            Step::Usage(usage) => self.send(acp::SessionUpdate::UsageUpdate(usage.clone())),
            Step::Notice(notice) => self.notice(notice),
            Step::Fail(fail) => {
                Flow::Fail(Error::new(fail.code, fail.message.as_str()).data(fail.data.clone()))
            }
        }
    }

    fn render<T: serde::de::DeserializeOwned + Clone>(
        &self,
        payload: &Payload<T>,
    ) -> Result<T, Flow> {
        payload.render(&self.vars).map_err(|err| {
            Flow::Fail(Error::new(
                ErrorCode::INTERNAL_ERROR,
                format!("scenario payload does not render to valid ACP: {err}"),
            ))
        })
    }

    /// Sends a `session/update` and records it for `session/load`.
    fn send(&self, update: acp::SessionUpdate) -> Flow {
        // Notices are explicitly not part of the session history.
        if !matches!(update, acp::SessionUpdate::Notice(_)) {
            self.agent.record(&self.session_id, update.clone());
        }
        let notification = acp::SessionNotification::new(self.session_id.clone(), update);
        match self.cx.send_notification(notification) {
            Ok(()) => Flow::Continue,
            Err(err) => Flow::Fail(err),
        }
    }

    async fn sleep(&self, duration: Duration) -> Flow {
        if self.cleanup {
            tokio::time::sleep(duration).await;
            return Flow::Continue;
        }
        tokio::select! {
            () = tokio::time::sleep(duration) => Flow::Continue,
            () = self.cancel.cancelled() => Flow::Stop(acp::StopReason::Cancelled),
        }
    }

    async fn stream(
        &self,
        text: &Payload<String>,
        wrap: fn(acp::ContentChunk) -> acp::SessionUpdate,
    ) -> Flow {
        let text = match self.render(text) {
            Ok(text) => text,
            Err(flow) => return flow,
        };
        for (index, chunk) in chunks(&text).into_iter().enumerate() {
            if index > 0
                && let Flow::Stop(reason) = self.sleep(CHUNK_DELAY).await
            {
                return Flow::Stop(reason);
            }
            match self.send(wrap(acp::ContentChunk::new(chunk.into()))) {
                Flow::Continue => {}
                other => return other,
            }
        }
        Flow::Continue
    }

    fn notice(&self, notice: &acp::Notice) -> Flow {
        let supported = self
            .client
            .session
            .as_ref()
            .is_some_and(|session| session.notices.is_some());
        if supported {
            return self.send(acp::SessionUpdate::Notice(notice.clone()));
        }
        let severity = match &notice.severity {
            acp::NoticeSeverity::Info => "info",
            acp::NoticeSeverity::Warning => "warning",
            acp::NoticeSeverity::Error => "error",
            acp::NoticeSeverity::Other(other) => other.as_str(),
            _ => "notice",
        };
        let mut text = format!("[{severity}] {}", notice.title);
        if let Some(description) = &notice.description {
            text.push_str(": ");
            text.push_str(description);
        }
        let chunk = acp::ContentChunk::new(text.into());
        self.send(acp::SessionUpdate::AgentMessageChunk(chunk))
    }

    async fn permission(&mut self, step: &PermissionStep) -> Flow {
        let tool_call = match self.render(&step.tool_call) {
            Ok(tool_call) => tool_call,
            Err(flow) => return flow,
        };
        let request = acp::RequestPermissionRequest::new(
            self.session_id.clone(),
            tool_call,
            step.options.clone(),
        );
        let branch = match self.ask(request).await {
            Ok(Some(response)) => match response.outcome {
                acp::RequestPermissionOutcome::Selected(selected) => selected.option_id.to_string(),
                _ => "cancelled".to_string(),
            },
            // Unanswered after cancellation: clean up as if it were `cancelled`.
            Ok(None) => "cancelled".to_string(),
            Err(flow) => return flow,
        };
        match step.on.get(&branch) {
            Some(steps) => self.run_branch(steps, branch == "cancelled").await,
            None => Flow::Continue,
        }
    }

    async fn elicitation(&mut self, step: &ElicitationStep) -> Flow {
        let request = match self.render(&step.request) {
            Ok(request) => request,
            Err(flow) => return flow,
        };
        let (supported, url_id) = {
            let caps = self.client.elicitation.as_ref();
            match &request.mode {
                acp::ElicitationMode::Form(_) => (
                    caps.is_some_and(acp::ElicitationCapabilities::supports_form),
                    None,
                ),
                acp::ElicitationMode::Url(url) => (
                    caps.is_some_and(acp::ElicitationCapabilities::supports_url),
                    Some(url.elicitation_id.clone()),
                ),
                _ => (false, None),
            }
        };
        if !supported {
            log!("client did not advertise this elicitation mode; taking the `cancel` branch");
            return self.run_branch(&step.on.cancel, true).await;
        }
        let action = match self.ask(request).await {
            Ok(Some(response)) => response.action,
            // Unanswered after cancellation: clean up as if it were `cancel`.
            Ok(None) => acp::ElicitationAction::Cancel,
            Err(flow) => return flow,
        };
        match action {
            acp::ElicitationAction::Accept(accept) => {
                self.set_elicitation_vars(accept.content.unwrap_or_default());
                if let Some(id) = url_id {
                    let complete = acp::CompleteElicitationNotification::new(id);
                    if let Err(err) = self.cx.send_notification(complete) {
                        return Flow::Fail(err);
                    }
                }
                self.run(&step.on.accept).await
            }
            acp::ElicitationAction::Decline => self.run(&step.on.decline).await,
            _ => self.run_branch(&step.on.cancel, true).await,
        }
    }

    fn set_elicitation_vars(
        &mut self,
        content: std::collections::BTreeMap<String, acp::ElicitationContentValue>,
    ) {
        self.vars.retain(|key, _| !key.starts_with("elicitation"));
        let whole = serde_json::to_string(&content).unwrap_or_default();
        self.vars.insert("elicitation".to_string(), whole);
        for (key, value) in content {
            let text = match value {
                acp::ElicitationContentValue::String(text) => text,
                other => serde_json::to_string(&other).unwrap_or_default(),
            };
            self.vars.insert(format!("elicitation.{key}"), text);
        }
    }

    /// Sends an agent-to-client request from the turn task. After the turn
    /// is cancelled the client still gets [`CANCEL_GRACE`] to answer it;
    /// `None` when it did not (the request is then withdrawn with
    /// `$/cancel_request`) or answered with an error.
    async fn ask<Req: JsonRpcRequest>(&self, request: Req) -> Result<Option<Req::Response>, Flow> {
        let method = request.method().to_string();
        let response = self.cx.send_request(request).block_task();
        tokio::pin!(response);
        let result = tokio::select! {
            result = &mut response => result,
            () = self.cancel.cancelled() => {
                match tokio::time::timeout(CANCEL_GRACE, &mut response).await {
                    Ok(result) => result,
                    Err(_) => {
                        log!("{method} unanswered {CANCEL_GRACE:?} after cancellation; withdrawing it");
                        return Ok(None);
                    }
                }
            }
        };
        match result {
            Ok(response) => Ok(Some(response)),
            Err(err) if self.cancel.is_cancelled() => {
                log!(
                    "{method} failed after cancellation: {} ({})",
                    err.message,
                    err.code
                );
                Ok(None)
            }
            Err(err) => Err(Flow::Fail(Error::new(
                ErrorCode::INTERNAL_ERROR,
                format!("{method} failed: {} ({})", err.message, err.code),
            ))),
        }
    }
}

/// Splits text into up to four chunks at whitespace boundaries.
fn chunks(text: &str) -> Vec<&str> {
    const MAX_CHUNKS: usize = 4;
    let words = text.split_inclusive(char::is_whitespace).count();
    let per_chunk = words.div_ceil(MAX_CHUNKS).max(1);
    let mut out = Vec::new();
    let (mut start, mut end) = (0, 0);
    for (index, word) in text.split_inclusive(char::is_whitespace).enumerate() {
        end += word.len();
        if (index + 1) % per_chunk == 0 {
            out.push(&text[start..end]);
            start = end;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn chunks_cover_the_text() {
        assert_eq!(chunks("You said: hello"), vec!["You ", "said: ", "hello"]);
        let long = "one two three four five six seven eight nine";
        let parts = chunks(long);
        assert!((2..=4).contains(&parts.len()), "{parts:?}");
        assert_eq!(parts.concat(), long);
        assert!(chunks("").is_empty());
    }
}
