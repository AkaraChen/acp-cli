//! The built-in demo turn end to end: keywords, the edit permission flow and
//! session modes.

mod common;

use agent_client_protocol::ErrorCode;
use agent_client_protocol::v1::schema as acp;
use pretty_assertions::assert_eq;

use common::MockAgent;
use common::message_text;
use common::select;
use common::tool_statuses;

use acp::ToolCallStatus::Completed;
use acp::ToolCallStatus::Failed;
use acp::ToolCallStatus::InProgress;
use acp::ToolCallStatus::Pending;

/// The statuses reported for the tool call whose id ends with `suffix`.
fn statuses_of(updates: &[acp::SessionUpdate], suffix: &str) -> Vec<Option<acp::ToolCallStatus>> {
    tool_statuses(updates)
        .into_iter()
        .filter(|(id, _)| id.ends_with(suffix))
        .map(|(_, status)| status)
        .collect()
}

fn tool_call<'a>(updates: &'a [acp::SessionUpdate], suffix: &str) -> &'a acp::ToolCall {
    updates
        .iter()
        .find_map(|update| match update {
            acp::SessionUpdate::ToolCall(call) if call.tool_call_id.0.ends_with(suffix) => {
                Some(call)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no `{suffix}` tool call in {updates:?}"))
}

#[tokio::test]
async fn demo_keywords_drive_tools_plans_and_stop_reasons() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            let id = conn.start(&cwd).await;
            conn.until_commands().await;

            let response = conn.prompt(&id, "tool read plan").await?;
            assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
            let updates = conn.drain();
            let exec = tool_call(&updates, "_exec");
            assert_eq!(exec.kind, acp::ToolKind::Execute);
            assert_eq!(
                statuses_of(&updates, "_exec"),
                vec![Some(InProgress), None, None, Some(Completed)]
            );
            let read = tool_call(&updates, "_read");
            assert_eq!(read.kind, acp::ToolKind::Read);
            assert_eq!(read.locations[0].path, cwd.join("README.md"));
            assert_eq!(
                statuses_of(&updates, "_read"),
                vec![Some(InProgress), Some(Completed)]
            );
            let plans: Vec<Vec<acp::PlanEntryStatus>> = updates
                .iter()
                .filter_map(|update| match update {
                    acp::SessionUpdate::Plan(plan) => Some(
                        plan.entries
                            .iter()
                            .map(|entry| entry.status.clone())
                            .collect(),
                    ),
                    _ => None,
                })
                .collect();
            use acp::PlanEntryStatus as S;
            assert_eq!(
                plans,
                vec![
                    vec![S::InProgress, S::Pending, S::Pending],
                    vec![S::Completed, S::InProgress, S::Pending],
                    vec![S::Completed, S::Completed, S::InProgress],
                    vec![S::Completed, S::Completed, S::Completed],
                ]
            );

            let response = conn.prompt(&id, "please refuse").await?;
            assert_eq!(response.stop_reason, acp::StopReason::Refusal);
            let text = message_text(&conn.drain());
            assert!(text.ends_with("Sorry, I can't help with that."), "{text}");

            let err = conn.prompt(&id, "error").await.expect_err("error keyword");
            assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
            let response = conn.prompt(&id, "hello again").await?;
            assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn demo_edit_asks_permission_unless_the_mode_accepts_edits() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            conn.initialize(acp::ClientCapabilities::new()).await;
            let session = conn.request(acp::NewSessionRequest::new(&cwd)).await?;
            let id = session.session_id;
            let modes = session.modes.expect("demo modes");
            assert_eq!(modes.current_mode_id.to_string(), "default");
            conn.until_commands().await;

            // reject_once: the edit fails.
            let prompt = conn.send_prompt(&id, "edit it");
            let (request, responder) = conn.next_permission().await;
            assert!(request.tool_call.tool_call_id.0.ends_with("_edit"));
            let options: Vec<String> = request
                .options
                .iter()
                .map(|option| option.option_id.to_string())
                .collect();
            assert_eq!(options, vec!["allow_once", "allow_always", "reject_once"]);
            responder.respond(select("reject_once"))?;
            assert_eq!(prompt.await?.stop_reason, acp::StopReason::EndTurn);
            let updates = conn.drain();
            let edit = tool_call(&updates, "_edit");
            assert_eq!(edit.kind, acp::ToolKind::Edit);
            assert!(
                matches!(&edit.content[..], [acp::ToolCallContent::Diff(diff)]
                    if diff.path == cwd.join("demo.txt")),
                "{edit:?}"
            );
            assert_eq!(
                statuses_of(&updates, "_edit"),
                vec![Some(Pending), Some(Failed)]
            );
            assert!(message_text(&updates).ends_with("OK, I left demo.txt alone."));

            // allow_once: the edit is applied.
            let prompt = conn.send_prompt(&id, "edit it");
            let (_request, responder) = conn.next_permission().await;
            responder.respond(select("allow_once"))?;
            prompt.await?;
            let updates = conn.drain();
            assert_eq!(
                statuses_of(&updates, "_edit"),
                vec![Some(Pending), Some(InProgress), Some(Completed)]
            );
            assert!(message_text(&updates).ends_with("Applied the edit to demo.txt."));

            // set_mode answers, then reports `current_mode_update`.
            conn.request(acp::SetSessionModeRequest::new(id.clone(), "accept_edits"))
                .await?;
            let update = conn.next_update().await;
            assert!(
                matches!(&update, acp::SessionUpdate::CurrentModeUpdate(mode)
                    if mode.current_mode_id.to_string() == "accept_edits"),
                "{update:?}"
            );
            let err = conn
                .request(acp::SetSessionModeRequest::new(id.clone(), "nope"))
                .await
                .expect_err("unknown mode");
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS);

            // In `accept_edits` the edit needs no permission.
            conn.prompt(&id, "edit it").await?;
            assert!(!conn.has_pending_requests());
            assert_eq!(
                statuses_of(&conn.drain(), "_edit"),
                vec![Some(Pending), Some(InProgress), Some(Completed)]
            );
            Ok(())
        })
        .await;
}
