//! Session lifecycle end to end: close and delete during a running turn,
//! cleanup when the client never answers a cancelled turn's request, and
//! agents sharing a state directory.

mod common;

use std::time::Duration;

use agent_client_protocol::ErrorCode;
use agent_client_protocol::v1::schema as acp;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use common::MockAgent;
use common::is_tool_status;
use common::ready;
use common::tool_statuses;

/// `deploy` asks permission; its `cancelled` branch takes a while to mark
/// the tool call failed.
fn slow_cleanup_scenario() -> Value {
    json!({
        "turns": [{"match": "deploy", "steps": [
            {"update": {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Deploy",
                        "status": "pending"}},
            {"permission": {
                "tool_call": {"toolCallId": "t1"},
                "on": {"cancelled": [
                    {"sleep_ms": 300},
                    {"update": {"sessionUpdate": "tool_call_update", "toolCallId": "t1",
                                "status": "failed"}}
                ]}
            }}
        ]}]
    })
}

fn cancelled() -> acp::RequestPermissionResponse {
    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled)
}

fn listed_ids(response: &acp::ListSessionsResponse) -> Vec<String> {
    let mut ids: Vec<String> = response
        .sessions
        .iter()
        .map(|session| session.session_id.to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn close_answers_after_the_cancelled_turn() {
    let agent = MockAgent::new(Some(slow_cleanup_scenario()));
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            let id = conn.start(&cwd).await;
            let prompt = conn.send_prompt(&id, "deploy");
            let (_request, responder) = conn.next_permission().await;
            let close = conn
                .cx
                .send_request(acp::CloseSessionRequest::new(id.clone()))
                .block_task();
            // Sent after the close, so the agent has cancelled the turn by then.
            responder.respond(cancelled())?;
            close.await?;

            // The turn had answered (and cleaned up) before the close response.
            let response = ready(prompt).await.expect("prompt answered before close")?;
            assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
            let updates = conn.drain();
            assert!(
                updates
                    .iter()
                    .any(is_tool_status("t1", acp::ToolCallStatus::Failed)),
                "{updates:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(conn.drain(), Vec::new(), "updates after session/close");

            let err = conn.prompt(&id, "hello").await.expect_err("closed");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            // Loading right away works: the turn is over.
            conn.request(acp::LoadSessionRequest::new(id.clone(), &cwd))
                .await?;
            let response = conn.prompt(&id, "hello").await?;
            assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn delete_cancels_the_turn_and_removes_the_session() {
    let agent = MockAgent::new(Some(slow_cleanup_scenario()));
    let cwd = agent.cwd().to_path_buf();
    let kept = agent
        .connect(async |mut conn| {
            let doomed = conn.start(&cwd).await;
            let kept = conn
                .request(acp::NewSessionRequest::new(&cwd))
                .await?
                .session_id;
            let all = conn.request(acp::ListSessionsRequest::new()).await?;
            let mut expected = vec![doomed.to_string(), kept.to_string()];
            expected.sort();
            assert_eq!(listed_ids(&all), expected);

            let prompt = conn.send_prompt(&doomed, "deploy");
            let (_request, responder) = conn.next_permission().await;
            let delete = conn
                .cx
                .send_request(acp::DeleteSessionRequest::new(doomed.clone()))
                .block_task();
            responder.respond(cancelled())?;
            delete.await?;
            let response = ready(prompt)
                .await
                .expect("prompt answered before delete")?;
            assert_eq!(response.stop_reason, acp::StopReason::Cancelled);

            let all = conn.request(acp::ListSessionsRequest::new()).await?;
            assert_eq!(listed_ids(&all), vec![kept.to_string()]);
            let err = conn
                .request(acp::DeleteSessionRequest::new(doomed.clone()))
                .await
                .expect_err("deleted twice");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            let err = conn.prompt(&doomed, "hello").await.expect_err("deleted");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            Ok(kept)
        })
        .await;

    // The deletion is persisted.
    agent
        .connect(async |conn| {
            conn.initialize(acp::ClientCapabilities::new()).await;
            let all = conn.request(acp::ListSessionsRequest::new()).await?;
            assert_eq!(listed_ids(&all), vec![kept.to_string()]);
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn unanswered_permission_still_runs_the_cancelled_branch() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            let id = conn.start(&cwd).await;
            let prompt = conn.send_prompt(&id, "edit");
            // Never answered: the agent gives up after its grace period.
            let (request, _responder) = conn.next_permission().await;
            let call_id = request.tool_call.tool_call_id.to_string();
            conn.cx
                .send_notification(acp::CancelNotification::new(id.clone()))?;
            let response = prompt.await?;
            assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
            let statuses: Vec<_> = tool_statuses(&conn.drain())
                .into_iter()
                .filter(|(id, _)| *id == call_id)
                .map(|(_, status)| status)
                .collect();
            assert_eq!(
                statuses,
                vec![
                    Some(acp::ToolCallStatus::Pending),
                    Some(acp::ToolCallStatus::Failed)
                ]
            );
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn agents_sharing_a_state_dir_keep_each_others_sessions() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    let ids = agent
        .connect(async |conn_a| {
            conn_a.initialize(acp::ClientCapabilities::new()).await;
            // B starts before A creates anything, so neither sees the other's
            // sessions in memory.
            let (a, b) = agent
                .connect(async |conn_b| {
                    conn_b.initialize(acp::ClientCapabilities::new()).await;
                    let a = conn_a
                        .request(acp::NewSessionRequest::new(&cwd))
                        .await?
                        .session_id;
                    let b = conn_b
                        .request(acp::NewSessionRequest::new(&cwd))
                        .await?
                        .session_id;
                    Ok((a, b))
                })
                .await;
            assert_ne!(a, b);
            // A writes again after B.
            conn_a.prompt(&a, "hello").await?;
            let mut ids = vec![a.to_string(), b.to_string()];
            ids.sort();
            Ok(ids)
        })
        .await;

    agent
        .connect(async |conn| {
            conn.initialize(acp::ClientCapabilities::new()).await;
            let all = conn.request(acp::ListSessionsRequest::new()).await?;
            assert_eq!(listed_ids(&all), ids);
            Ok(())
        })
        .await;
}
