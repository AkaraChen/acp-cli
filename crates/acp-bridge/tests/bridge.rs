//! End-to-end: app-server client ⇄ bridge ⇄ ACP mock agent.

mod common;

use common::Harness;
use common::HarnessConfig;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn item_type(message: &Value) -> &str {
    message["params"]["item"]["type"].as_str().unwrap_or("")
}

/// Collect `item/completed` notifications until the turn completes.
async fn completed_items(harness: &mut Harness) -> (Vec<Value>, Value) {
    let mut items = Vec::new();
    loop {
        let item = harness.next_any(&["item/completed", "turn/completed"]).await;
        if item["method"] == "turn/completed" {
            return (items, item);
        }
        items.push(item["params"]["item"].clone());
    }
}

#[tokio::test]
async fn streamed_turn_produces_items_and_completes() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    let turn = harness.send_turn(&thread_id, "hello").await;
    assert_eq!(turn["turn"]["status"], "inProgress");

    let methods = harness.methods_until("turn/completed").await;
    assert!(methods.contains(&"turn/started".to_string()), "{methods:?}");
    assert!(methods.contains(&"item/agentMessage/delta".to_string()), "{methods:?}");
    let started = methods.iter().position(|m| m == "turn/started");
    let first_delta = methods.iter().position(|m| m == "item/agentMessage/delta");
    assert!(started < first_delta, "{methods:?}");

    let read = harness
        .request("thread/read", json!({"threadId": thread_id, "includeTurns": true}))
        .await
        .expect("thread/read");
    let turns = read["thread"]["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 1);
    let texts: Vec<&str> = turns[0]["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["type"] == "agentMessage")
        .filter_map(|item| item["text"].as_str())
        .collect();
    assert!(texts.iter().any(|text| text.contains("You said: hello")), "{texts:?}");
    assert_eq!(turns[0]["status"], "completed");
}

#[tokio::test]
async fn interrupt_ends_the_turn_as_interrupted() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    let turn = harness.send_turn(&thread_id, "slow").await;
    let turn_id = turn["turn"]["id"].as_str().expect("turn id").to_string();
    harness.next("item/agentMessage/delta").await;
    harness
        .request("turn/interrupt", json!({"threadId": thread_id, "turnId": turn_id}))
        .await
        .expect("turn/interrupt");
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "interrupted");
}

#[tokio::test]
async fn tool_calls_become_command_items() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "run a tool").await;
    let (items, _) = completed_items(&mut harness).await;
    let command = items
        .iter()
        .find(|item| item["type"] == "commandExecution")
        .expect("command item");
    assert_eq!(command["status"], "completed");
    assert!(command["aggregatedOutput"].as_str().is_some_and(|out| !out.is_empty()));
}

#[tokio::test]
async fn read_tool_calls_become_read_actions() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "read the readme").await;
    let (items, _) = completed_items(&mut harness).await;
    let read = items
        .iter()
        .find(|item| item["type"] == "commandExecution")
        .expect("read item");
    assert_eq!(read["commandActions"][0]["type"], "read");
}

#[tokio::test]
async fn edit_permission_accept_and_decline() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;

    harness.send_turn(&thread_id, "edit please").await;
    let request = harness.next("item/fileChange/requestApproval").await;
    // The file change item is announced before its approval request.
    let started = loop {
        let started = harness.next("item/started").await;
        if item_type(&started) == "fileChange" {
            break started;
        }
    };
    assert_eq!(started["params"]["item"]["id"], request["params"]["itemId"]);
    harness
        .respond(request["id"].clone(), json!({"decision": "accept"}))
        .await;
    harness.next("serverRequest/resolved").await;
    let (items, _) = completed_items(&mut harness).await;
    let change = items.iter().find(|item| item["type"] == "fileChange").expect("file change");
    assert_eq!(change["status"], "completed");
    assert!(change["changes"][0]["diff"].as_str().is_some_and(|diff| diff.contains("+c")));

    harness.send_turn(&thread_id, "edit again").await;
    let request = harness.next("item/fileChange/requestApproval").await;
    harness
        .respond(request["id"].clone(), json!({"decision": "decline"}))
        .await;
    let (items, _) = completed_items(&mut harness).await;
    let change = items.iter().find(|item| item["type"] == "fileChange").expect("file change");
    assert_eq!(change["status"], "failed");
}

#[tokio::test]
async fn interrupt_resolves_pending_permission() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    let turn = harness.send_turn(&thread_id, "edit please").await;
    let turn_id = turn["turn"]["id"].as_str().expect("turn id").to_string();
    let request = harness.next("item/fileChange/requestApproval").await;
    harness
        .request("turn/interrupt", json!({"threadId": thread_id, "turnId": turn_id}))
        .await
        .expect("interrupt");
    let resolved = harness.next("serverRequest/resolved").await;
    assert_eq!(resolved["params"]["requestId"], request["id"]);
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "interrupted");
}

fn scenario_with_models() -> Value {
    json!({
        "config_options": [
            {
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": "small",
                "options": [
                    {"value": "small", "name": "Small"},
                    {"value": "large", "name": "Large"}
                ]
            },
            {
                "id": "effort",
                "name": "Effort",
                "category": "thought_level",
                "type": "select",
                "currentValue": "low",
                "options": [
                    {"value": "low", "name": "Low"},
                    {"value": "high", "name": "High"}
                ]
            }
        ]
    })
}

#[tokio::test]
async fn model_list_and_settings_follow_config_options() {
    let mut harness = Harness::start(HarnessConfig {
        scenario: Some(scenario_with_models()),
        ..HarnessConfig::default()
    })
    .await;
    let thread_id = harness.start_thread().await;
    let models = harness.request("model/list", json!({})).await.expect("model/list");
    let ids: Vec<&str> = models["data"]
        .as_array()
        .expect("data")
        .iter()
        .filter_map(|model| model["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["small", "large"]);
    assert_eq!(models["data"][0]["isDefault"], true);

    harness
        .request(
            "thread/settings/update",
            json!({"threadId": thread_id, "model": "large", "effort": "high"}),
        )
        .await
        .expect("settings");
    let models = harness.request("model/list", json!({})).await.expect("model/list");
    assert_eq!(models["data"][1]["isDefault"], true);
    let read = harness
        .request("thread/read", json!({"threadId": thread_id, "includeTurns": false}))
        .await
        .expect("read");
    assert_eq!(read["thread"]["model"], "large");
}

fn scenario_with_history() -> Value {
    json!({
        "sessions": [{
            "session_id": "old-1",
            "cwd": "/",
            "title": "Earlier work",
            "updated_at": "2026-01-01T00:00:00Z",
            "history": [
                {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "first question"}},
                {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "first answer"}},
                {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "second question"}},
                {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": "completed"},
                {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "second answer"}}
            ]
        }]
    })
}

#[tokio::test]
async fn list_and_resume_rebuild_history() {
    let mut harness = Harness::start(HarnessConfig {
        scenario: Some(scenario_with_history()),
        ..HarnessConfig::default()
    })
    .await;
    let list = harness.request("thread/list", json!({})).await.expect("thread/list");
    let threads = list["data"].as_array().expect("data");
    let old = threads
        .iter()
        .find(|thread| thread["name"] == "Earlier work")
        .expect("listed session");
    let thread_id = old["id"].as_str().expect("id").to_string();

    let resumed = harness
        .request("thread/resume", json!({"threadId": thread_id}))
        .await
        .expect("thread/resume");
    let turns = resumed["thread"]["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 2, "{turns:#?}");
    let kinds = |turn: &Value| -> Vec<String> {
        turn["items"]
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["type"].as_str().unwrap_or("").to_string())
            .collect()
    };
    assert_eq!(kinds(&turns[0]), vec!["userMessage", "agentMessage"]);
    assert_eq!(kinds(&turns[1]), vec!["userMessage", "commandExecution", "agentMessage"]);

    // The resumed thread accepts new turns.
    harness.send_turn(&thread_id, "hello again").await;
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "completed");
}

#[tokio::test]
async fn elicitation_round_trip() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "ask me").await;
    let request = harness.next("mcpServer/elicitation/request").await;
    assert_eq!(request["params"]["mode"], "form");
    harness
        .respond(
            request["id"].clone(),
            json!({"action": "accept", "content": {"name": "Ada", "agree": true}, "_meta": null}),
        )
        .await;
    let (items, _) = completed_items(&mut harness).await;
    let text: String = items
        .iter()
        .filter(|item| item["type"] == "agentMessage")
        .filter_map(|item| item["text"].as_str())
        .collect();
    assert!(text.contains("Ada"), "{text}");
}

#[tokio::test]
async fn refusal_completes_with_a_warning() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "refuse this").await;
    let warning = harness.next("warning").await;
    assert!(warning["params"]["message"].as_str().is_some_and(|m| m.contains("refused")));
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "completed");
}

#[tokio::test]
async fn prompt_error_fails_the_turn() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "error out").await;
    let error = harness.next("error").await;
    assert!(error["params"]["error"]["message"].as_str().is_some());
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "failed");
}

#[tokio::test]
async fn ephemeral_threads_are_rejected() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let error = harness
        .request("thread/start", json!({"ephemeral": true}))
        .await
        .expect_err("ephemeral thread/start must fail");
    assert_eq!(error["code"], -32600);
}

#[tokio::test]
async fn unsupported_requests_are_method_not_found() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let error = harness
        .request("thread/fork", json!({"threadId": "x"}))
        .await
        .expect_err("fork is unsupported");
    assert_eq!(error["code"], -32601);
}
