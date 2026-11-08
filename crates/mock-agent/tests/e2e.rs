//! End-to-end tests: the real binary driven by the SDK client over stdio.

mod common;

use std::time::Duration;
use std::time::Instant;

use agent_client_protocol::ErrorCode;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema as acp;
use pretty_assertions::assert_eq;
use serde_json::json;

use common::MockAgent;
use common::is_message_chunk;
use common::message_text;
use common::select;

fn boolean_config_caps() -> acp::ClientCapabilities {
    acp::ClientCapabilities::new().session(
        acp::ClientSessionCapabilities::new().config_options(
            acp::SessionConfigOptionsCapabilities::new()
                .boolean(acp::BooleanConfigOptionCapabilities::new()),
        ),
    )
}

#[tokio::test]
async fn initialize_advertises_capabilities() {
    let agent = MockAgent::new(None);
    let response = agent
        .connect(async |conn| Ok(conn.initialize(acp::ClientCapabilities::new()).await))
        .await;
    assert_eq!(response.protocol_version, ProtocolVersion::V1);
    let caps = &response.agent_capabilities;
    assert!(caps.load_session);
    assert!(caps.prompt_capabilities.image && caps.prompt_capabilities.embedded_context);
    assert!(!caps.mcp_capabilities.http && !caps.mcp_capabilities.sse);
    let sessions = &caps.session_capabilities;
    assert!(sessions.list.is_some() && sessions.resume.is_some());
    assert!(sessions.close.is_some() && sessions.delete.is_some());
    assert!(caps.auth.logout.is_some());
    assert!(response.auth_methods.is_empty());
    let info = response.agent_info.expect("agent info");
    assert_eq!(info.name, "acpc-mock-agent");
}

#[tokio::test]
async fn hello_streams_message_chunks_then_end_turn() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    let (response, updates) = agent
        .connect(async |mut conn| {
            let session_id = conn.start(&cwd).await;
            let commands = conn.next_update().await;
            assert!(
                matches!(commands, acp::SessionUpdate::AvailableCommandsUpdate(_)),
                "{commands:?}"
            );
            let response = conn.prompt(&session_id, "hello").await?;
            Ok((response, conn.drain()))
        })
        .await;
    assert_eq!(response.stop_reason, acp::StopReason::EndTurn);
    let chunks = updates
        .iter()
        .filter(|update| is_message_chunk(update))
        .count();
    assert!(chunks >= 2, "expected several chunks, got {updates:?}");
    assert!(message_text(&updates).starts_with("You said: hello"));
    assert!(matches!(
        updates.last(),
        Some(acp::SessionUpdate::UsageUpdate(_))
    ));
}

#[tokio::test]
async fn slow_prompt_is_cancelled() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    let (response, elapsed) = agent
        .connect(async |mut conn| {
            let session_id = conn.start(&cwd).await;
            let started = Instant::now();
            let prompt = conn.cx.send_request(acp::PromptRequest::new(
                session_id.clone(),
                vec!["go slow".into()],
            ));
            conn.wait_for(|update| message_text(std::slice::from_ref(update)) == " 1")
                .await;
            conn.cx
                .send_notification(acp::CancelNotification::new(session_id))?;
            let response = prompt.block_task().await?;
            let elapsed = started.elapsed();
            conn.drain();
            // The demo sleeps 100ms between counts; give a stray one time to show.
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(conn.drain(), Vec::new(), "updates after the response");
            Ok((response, elapsed))
        })
        .await;
    assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
    assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
}

#[tokio::test]
async fn set_config_option_round_trips() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            conn.initialize(acp::ClientCapabilities::new()).await;
            let session = conn.request(acp::NewSessionRequest::new(&cwd)).await?;
            let ids: Vec<String> = session
                .config_options
                .iter()
                .flatten()
                .map(|option| option.id.to_string())
                .collect();
            assert_eq!(
                ids,
                vec!["model", "reasoning"],
                "booleans need the client capability"
            );

            let request = acp::SetSessionConfigOptionRequest::new(
                session.session_id.clone(),
                "model",
                "mock-smart",
            );
            let response = conn.request(request).await?;
            let current = |options: &[acp::SessionConfigOption]| match &options[0].kind {
                acp::SessionConfigKind::Select(select) => select.current_value.to_string(),
                other => panic!("unexpected kind {other:?}"),
            };
            assert_eq!(current(&response.config_options), "mock-smart");
            let update = conn
                .wait_for(|update| matches!(update, acp::SessionUpdate::ConfigOptionUpdate(_)))
                .await;
            let acp::SessionUpdate::ConfigOptionUpdate(update) = update else {
                unreachable!()
            };
            assert_eq!(update.config_options, response.config_options);

            let bad = acp::SetSessionConfigOptionRequest::new(session.session_id, "model", "nope");
            let err = conn.request(bad).await.expect_err("unknown value");
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
            Ok(())
        })
        .await;

    // Clients advertising boolean config options also get the toggle.
    agent
        .connect(async |conn| {
            conn.initialize(boolean_config_caps()).await;
            let session = conn.request(acp::NewSessionRequest::new(&cwd)).await?;
            assert_eq!(session.config_options.map(|options| options.len()), Some(3));
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn list_and_load_replay_history() {
    let agent = MockAgent::new(Some(json!({
        "sessions": [{
            "session_id": "seeded",
            "cwd": "/somewhere/else",
            "title": "Seeded",
            "updated_at": "2026-01-01T00:00:00Z",
            "history": [{"sessionUpdate": "notice", "severity": "info", "title": "live only"},
                        {"sessionUpdate": "agent_message_chunk",
                         "content": {"type": "text", "text": "from the seed"}}]
        }]
    })));
    let cwd = agent.cwd().to_path_buf();
    let (session_id, live) = agent
        .connect(async |mut conn| {
            let session_id = conn.start(&cwd).await;
            conn.prompt(&session_id, "hello").await?;
            let live: Vec<_> = conn
                .drain()
                .into_iter()
                .filter(|update| !matches!(update, acp::SessionUpdate::AvailableCommandsUpdate(_)))
                .collect();
            Ok((session_id, live))
        })
        .await;

    // A new process sees the persisted session and replays it on load.
    agent
        .connect(async |mut conn| {
            conn.initialize(acp::ClientCapabilities::new()).await;
            let all = conn.request(acp::ListSessionsRequest::new()).await?;
            let ids: Vec<String> = all
                .sessions
                .iter()
                .map(|s| s.session_id.to_string())
                .collect();
            assert_eq!(ids, vec![session_id.to_string(), "seeded".to_string()]);
            assert_eq!(all.next_cursor, None);
            let filtered = conn
                .request(acp::ListSessionsRequest::new().cwd(cwd.clone()))
                .await?;
            assert_eq!(filtered.sessions.len(), 1);
            assert_eq!(filtered.sessions[0].title.as_deref(), Some("hello"));

            conn.request(acp::LoadSessionRequest::new(session_id.clone(), &cwd))
                .await?;
            let replayed = replayed_before_response(&mut conn).await;
            let user = acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new("hello".into()));
            assert_eq!(replayed[0], user);
            assert_eq!(&replayed[1..], &live[..]);

            // Notices are live events, so the seeded one is not replayed.
            conn.request(acp::LoadSessionRequest::new("seeded", &cwd))
                .await?;
            let replayed = replayed_before_response(&mut conn).await;
            let chunk = acp::ContentChunk::new("from the seed".into());
            assert_eq!(replayed, vec![acp::SessionUpdate::AgentMessageChunk(chunk)]);

            // resume does not replay
            conn.request(acp::ResumeSessionRequest::new(session_id.clone(), &cwd))
                .await?;
            assert_eq!(conn.until_commands().await, Vec::new());
            Ok(())
        })
        .await;
}

/// The updates that arrived before the `session/load` response just
/// received, checking that `available_commands_update` only follows it.
async fn replayed_before_response(conn: &mut common::Conn) -> Vec<acp::SessionUpdate> {
    // The client dispatches in order, so the replay is already buffered.
    let mut replayed = conn.drain();
    if matches!(
        replayed.last(),
        Some(acp::SessionUpdate::AvailableCommandsUpdate(_))
    ) {
        replayed.pop();
    } else {
        assert_eq!(conn.until_commands().await, Vec::new());
    }
    let commands = replayed
        .iter()
        .filter(|update| matches!(update, acp::SessionUpdate::AvailableCommandsUpdate(_)))
        .count();
    assert_eq!(
        commands, 0,
        "commands update before the response: {replayed:?}"
    );
    replayed
}

#[tokio::test]
async fn scenario_permission_branches_and_cancellation() {
    let agent = MockAgent::new(Some(json!({
        "turns": [{
            "match": "deploy",
            "steps": [
                {"update": {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Deploy",
                            "kind": "execute", "locations": [{"path": "{{cwd}}/app"}]}},
                {"permission": {
                    "tool_call": {"toolCallId": "t1"},
                    "options": [{"optionId": "yes", "name": "Yes", "kind": "allow_once"},
                                {"optionId": "no", "name": "No", "kind": "reject_once"}],
                    "on": {"yes": [{"text": "deployed"}], "cancelled": [{"text": "aborted"}]}
                }}
            ]
        }]
    })));
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            let session_id = conn.start(&cwd).await;
            let prompt = conn.cx.send_request(acp::PromptRequest::new(
                session_id.clone(),
                vec!["please deploy".into()],
            ));
            let (request, responder) = conn.next_permission().await;
            assert_eq!(request.tool_call.tool_call_id.to_string(), "t1");
            responder.respond(select("yes"))?;
            assert_eq!(
                prompt.block_task().await?.stop_reason,
                acp::StopReason::EndTurn
            );
            let updates = conn.drain();
            assert_eq!(message_text(&updates), "deployed");
            let Some(acp::SessionUpdate::ToolCall(call)) = updates.get(1) else {
                panic!("expected the tool call after the commands update: {updates:?}");
            };
            assert_eq!(call.locations[0].path, cwd.join("app"));

            // session/cancel while the permission is pending: the client answers
            // `cancelled` and the turn stops with `cancelled`.
            let prompt = conn.cx.send_request(acp::PromptRequest::new(
                session_id.clone(),
                vec!["deploy again".into()],
            ));
            let (_request, responder) = conn.next_permission().await;
            conn.cx
                .send_notification(acp::CancelNotification::new(session_id))?;
            responder.respond(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Cancelled,
            ))?;
            assert_eq!(
                prompt.block_task().await?.stop_reason,
                acp::StopReason::Cancelled
            );
            assert_eq!(message_text(&conn.drain()), "aborted");
            Ok(())
        })
        .await;
}

fn elicitation_caps(form: bool, url: bool) -> acp::ClientCapabilities {
    let mut caps = acp::ElicitationCapabilities::new();
    if form {
        caps = caps.form(acp::ElicitationFormCapabilities::new());
    }
    if url {
        caps = caps.url(acp::ElicitationUrlCapabilities::new());
    }
    acp::ClientCapabilities::new().elicitation(caps)
}

fn answer(action: acp::ElicitationAction) -> acp::CreateElicitationResponse {
    acp::CreateElicitationResponse::new(action)
}

fn accept_with(fields: &[(&str, acp::ElicitationContentValue)]) -> acp::CreateElicitationResponse {
    let content: std::collections::BTreeMap<_, _> = fields
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect();
    answer(acp::ElicitationAction::Accept(
        acp::ElicitationAcceptAction::new().content(content),
    ))
}

#[tokio::test]
async fn demo_elicitation_echoes_answers() {
    let agent = MockAgent::new(None);
    let cwd = agent.cwd().to_path_buf();
    let (accepted, declined) = agent
        .connect(async |mut conn| {
            conn.initialize(elicitation_caps(true, false)).await;
            let session = conn.request(acp::NewSessionRequest::new(&cwd)).await?;
            let id = session.session_id;

            let prompt = conn.send_prompt(&id, "ask me");
            let (request, responder) = conn.next_elicitation().await;
            assert!(matches!(request.mode, acp::ElicitationMode::Form(_)));
            responder.respond(accept_with(&[
                ("name", "Ada".into()),
                ("agree", true.into()),
            ]))?;
            prompt.await?;
            let accepted = message_text(&conn.drain());

            let prompt = conn.send_prompt(&id, "ask again");
            let (_request, responder) = conn.next_elicitation().await;
            responder.respond(answer(acp::ElicitationAction::Decline))?;
            prompt.await?;
            Ok((accepted, message_text(&conn.drain())))
        })
        .await;
    assert!(
        accepted.contains(r#"{"agree":true,"name":"Ada"}"#),
        "{accepted}"
    );
    assert!(declined.ends_with("You declined the form."), "{declined}");
}

#[tokio::test]
async fn scenario_elicitation_branches_and_url_mode() {
    let agent = MockAgent::new(Some(json!({
        "turns": [
            {"match": "form", "steps": [{
                "elicitation": {"mode": "form", "message": "Who?",
                                "requestedSchema": {"type": "object",
                                                    "properties": {"name": {"type": "string"}}}},
                "on": {"accept": [{"text": "hi {{elicitation.name}}"}],
                       "decline": [{"text": "declined"}],
                       "cancel": [{"text": "cancelled"}]}
            }]},
            {"match": "url", "steps": [{
                "elicitation": {"mode": "url", "elicitationId": "e1",
                                "url": "https://example.com/login", "message": "Sign in"},
                "on": {"accept": [{"text": "signed in"}], "cancel": [{"text": "cancelled"}]}
            }]}
        ]
    })));
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            conn.initialize(elicitation_caps(true, true)).await;
            let id = conn
                .request(acp::NewSessionRequest::new(&cwd))
                .await?
                .session_id;
            let run = async |conn: &mut common::Conn, reply: acp::CreateElicitationResponse| {
                let prompt = conn.send_prompt(&id, "form");
                let (_request, responder) = conn.next_elicitation().await;
                responder.respond(reply)?;
                assert_eq!(prompt.await?.stop_reason, acp::StopReason::EndTurn);
                Ok::<_, agent_client_protocol::Error>(message_text(&conn.drain()))
            };
            let declined = run(&mut conn, answer(acp::ElicitationAction::Decline)).await?;
            assert_eq!(declined, "declined");
            let cancelled = run(&mut conn, answer(acp::ElicitationAction::Cancel)).await?;
            assert_eq!(cancelled, "cancelled");
            let accepted = run(&mut conn, accept_with(&[("name", "Ada".into())])).await?;
            assert_eq!(accepted, "hi Ada");

            // URL mode: after the user accepts, the agent reports completion.
            let prompt = conn.send_prompt(&id, "url");
            let (request, responder) = conn.next_elicitation().await;
            let acp::ElicitationMode::Url(url) = &request.mode else {
                panic!("expected URL mode: {request:?}");
            };
            assert_eq!(url.url, "https://example.com/login");
            responder.respond(answer(acp::ElicitationAction::Accept(
                acp::ElicitationAcceptAction::new(),
            )))?;
            let complete = conn.next_completion().await;
            assert_eq!(complete.elicitation_id.to_string(), "e1");
            prompt.await?;
            assert_eq!(message_text(&conn.drain()), "signed in");
            Ok(())
        })
        .await;

    // A mode the client did not advertise is never sent: the `cancel` branch runs.
    for (caps, prompt) in [
        (elicitation_caps(true, false), "url"),
        (acp::ClientCapabilities::new(), "form"),
    ] {
        agent
            .connect(async |mut conn| {
                conn.initialize(caps).await;
                let id = conn
                    .request(acp::NewSessionRequest::new(&cwd))
                    .await?
                    .session_id;
                conn.until_commands().await;
                conn.prompt(&id, prompt).await?;
                assert_eq!(message_text(&conn.drain()), "cancelled");
                assert!(!conn.has_pending_requests());
                Ok(())
            })
            .await;
    }
}

#[tokio::test]
async fn errors_follow_the_spec() {
    let agent = MockAgent::new(Some(json!({
        "capabilities": {"list": false, "load_session": false},
        "turns": [{"match": "boom", "steps": [{"fail": {"code": -32603, "message": "boom"}}]}]
    })));
    let cwd = agent.cwd().to_path_buf();
    agent
        .connect(async |mut conn| {
            let session_id = conn.start(&cwd).await;
            let err = conn
                .prompt(&session_id, "boom")
                .await
                .expect_err("fail step");
            assert_eq!(
                (err.code, err.message.as_str()),
                (ErrorCode::INTERNAL_ERROR, "boom")
            );
            // The failed turn is over; the session accepts new prompts.
            conn.prompt(&session_id, "hi").await?;

            let unknown = acp::SessionId::new("nope");
            let err = conn
                .prompt(&unknown, "hi")
                .await
                .expect_err("unknown session");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            let err = conn
                .request(acp::CloseSessionRequest::new(unknown.clone()))
                .await
                .expect_err("close unknown");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            let err = conn
                .request(acp::DeleteSessionRequest::new(unknown))
                .await
                .expect_err("delete unknown");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            let err = conn
                .request(acp::ListSessionsRequest::new())
                .await
                .expect_err("list not advertised");
            assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
            let err = conn
                .request(acp::LoadSessionRequest::new(session_id.clone(), &cwd))
                .await
                .expect_err("load not advertised");
            assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);

            conn.request(acp::CloseSessionRequest::new(session_id.clone()))
                .await?;
            let err = conn.prompt(&session_id, "hi").await.expect_err("closed");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            // Only sessions active on this connection can be closed.
            let err = conn
                .request(acp::CloseSessionRequest::new(session_id.clone()))
                .await
                .expect_err("already closed");
            assert_eq!(err.code, ErrorCode::RESOURCE_NOT_FOUND);
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn scripted_notice_usage_and_wait_for_cancel() {
    let agent = MockAgent::new(Some(json!({
        "turns": [{"match": "wait", "steps": [
            {"notice": {"severity": "warning", "title": "careful"}},
            {"usage": {"used": 5, "size": 10}},
            {"wait_for_cancel": true},
            {"text": "never sent"}
        ]}]
    })));
    let cwd = agent.cwd().to_path_buf();
    let is_usage =
        |update: &acp::SessionUpdate| matches!(update, acp::SessionUpdate::UsageUpdate(_));
    agent
        .connect(async |mut conn| {
            // Without the `session.notices` capability a notice becomes text.
            let session_id = conn.start(&cwd).await;
            conn.until_commands().await;
            let prompt = conn.cx.send_request(acp::PromptRequest::new(
                session_id.clone(),
                vec!["wait".into()],
            ));
            let before = conn.collect_until(is_usage).await;
            assert_eq!(message_text(&before), "[warning] careful");
            assert!(
                matches!(before.last(), Some(acp::SessionUpdate::UsageUpdate(u)) if u.used == 5)
            );
            conn.cx
                .send_notification(acp::CancelNotification::new(session_id.clone()))?;
            let response = prompt.block_task().await?;
            assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
            assert_eq!(conn.drain(), Vec::new(), "nothing after the wait");

            // `$/cancel_request` on the prompt request also ends the turn.
            let prompt = conn.cx.send_request(acp::PromptRequest::new(
                session_id.clone(),
                vec!["wait again".into()],
            ));
            conn.collect_until(is_usage).await;
            prompt.cancel()?;
            let response = prompt.block_task().await?;
            assert_eq!(response.stop_reason, acp::StopReason::Cancelled);
            Ok(())
        })
        .await;

    // Clients advertising notices get the real `notice` update.
    agent
        .connect(async |mut conn| {
            let caps = acp::ClientCapabilities::new().session(
                acp::ClientSessionCapabilities::new().notices(acp::NoticeCapabilities::new()),
            );
            conn.initialize(caps).await;
            let session = conn.request(acp::NewSessionRequest::new(&cwd)).await?;
            conn.until_commands().await;
            let prompt = conn.cx.send_request(acp::PromptRequest::new(
                session.session_id.clone(),
                vec!["wait".into()],
            ));
            let before = conn.collect_until(is_usage).await;
            assert!(matches!(&before[0], acp::SessionUpdate::Notice(n) if n.title == "careful"));
            conn.cx
                .send_notification(acp::CancelNotification::new(session.session_id))?;
            prompt.block_task().await?;
            Ok(())
        })
        .await;
}
