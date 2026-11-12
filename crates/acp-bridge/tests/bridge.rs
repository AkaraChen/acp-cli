//! End-to-end: app-server client ⇄ bridge ⇄ ACP mock agent.

mod common;

use std::time::Duration;

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
        let item = harness
            .next_any(&["item/completed", "turn/completed"])
            .await;
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
    assert!(
        methods.contains(&"item/agentMessage/delta".to_string()),
        "{methods:?}"
    );
    let started = methods.iter().position(|m| m == "turn/started");
    let first_delta = methods.iter().position(|m| m == "item/agentMessage/delta");
    assert!(started < first_delta, "{methods:?}");

    let read = harness
        .request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )
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
    assert!(
        texts.iter().any(|text| text.contains("You said: hello")),
        "{texts:?}"
    );
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
        .request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
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
    assert!(
        command["aggregatedOutput"]
            .as_str()
            .is_some_and(|out| !out.is_empty())
    );
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
    let change = items
        .iter()
        .find(|item| item["type"] == "fileChange")
        .expect("file change");
    assert_eq!(change["status"], "completed");
    assert!(
        change["changes"][0]["diff"]
            .as_str()
            .is_some_and(|diff| diff.contains("+c"))
    );

    harness.send_turn(&thread_id, "edit again").await;
    let request = harness.next("item/fileChange/requestApproval").await;
    harness
        .respond(request["id"].clone(), json!({"decision": "decline"}))
        .await;
    let (items, _) = completed_items(&mut harness).await;
    let change = items
        .iter()
        .find(|item| item["type"] == "fileChange")
        .expect("file change");
    assert_eq!(change["status"], "failed");
}

#[tokio::test]
async fn default_policy_is_not_yolo() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let started = harness
        .request("thread/start", json!({}))
        .await
        .expect("thread/start");
    assert_eq!(started["approvalPolicy"], "on-request");
    assert_eq!(started["sandbox"]["type"], "externalSandbox");
}

#[tokio::test]
async fn yolo_approves_permissions_but_still_asks_elicitations() {
    let mut harness = Harness::start(HarnessConfig {
        yolo: true,
        ..HarnessConfig::default()
    })
    .await;
    let started = harness
        .request("thread/start", json!({}))
        .await
        .expect("thread/start");
    assert_eq!(started["approvalPolicy"], "never");
    assert_eq!(started["sandbox"]["type"], "dangerFullAccess");
    let thread_id = started["thread"]["id"]
        .as_str()
        .expect("thread id")
        .to_string();

    harness.send_turn(&thread_id, "edit please").await;
    let methods = harness.methods_until("turn/completed").await;
    assert!(
        !methods
            .iter()
            .any(|method| method.ends_with("/requestApproval")),
        "{methods:?}"
    );
    let read = harness
        .request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )
        .await
        .expect("thread/read");
    let items = read["thread"]["turns"][0]["items"]
        .as_array()
        .expect("items");
    let change = items
        .iter()
        .find(|item| item["type"] == "fileChange")
        .expect("file change");
    assert_eq!(change["status"], "completed");

    harness.send_turn(&thread_id, "ask me").await;
    let request = harness.next("mcpServer/elicitation/request").await;
    assert_eq!(request["params"]["mode"], "form");
}

#[tokio::test]
async fn interrupt_resolves_pending_permission() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    let turn = harness.send_turn(&thread_id, "edit please").await;
    let turn_id = turn["turn"]["id"].as_str().expect("turn id").to_string();
    let request = harness.next("item/fileChange/requestApproval").await;
    harness
        .request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
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
    let models = harness
        .request("model/list", json!({}))
        .await
        .expect("model/list");
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
    let models = harness
        .request("model/list", json!({}))
        .await
        .expect("model/list");
    assert_eq!(models["data"][1]["isDefault"], true);
    let read = harness
        .request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": false}),
        )
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
    let list = harness
        .request("thread/list", json!({}))
        .await
        .expect("thread/list");
    let threads = list["data"].as_array().expect("data");
    let old = threads
        .iter()
        .find(|thread| thread["name"] == "Earlier work")
        .expect("listed session");
    let thread_id = old["id"].as_str().expect("id").to_string();
    for (cwd, listed) in [
        (json!("/"), true),
        (json!("/elsewhere"), false),
        (json!(["/elsewhere", "/"]), true),
        (json!(["/elsewhere", "/other"]), false),
    ] {
        let list = harness
            .request("thread/list", json!({"cwd": cwd}))
            .await
            .expect("thread/list");
        let found = list["data"]
            .as_array()
            .expect("data")
            .iter()
            .any(|thread| thread["id"] == thread_id.as_str());
        assert_eq!(found, listed, "cwd filter {cwd}");
    }

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
    assert_eq!(
        kinds(&turns[1]),
        vec!["userMessage", "commandExecution", "agentMessage"]
    );

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
    assert!(
        warning["params"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("refused"))
    );
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
        .request("thread/rollback", json!({"threadId": "x", "numTurns": 1}))
        .await
        .expect_err("rollback is unsupported");
    assert_eq!(error["code"], -32601);
}

fn locked_scenario() -> Value {
    json!({
        "require_auth": true,
        "auth_methods": [{"id": "login", "name": "Log in"}]
    })
}

fn two_agents() -> HarnessConfig {
    HarnessConfig {
        agents: vec![
            ("mock".to_string(), "Mock".to_string()),
            ("locked".to_string(), "Locked".to_string()),
        ],
        agent_scenarios: vec![("locked".to_string(), locked_scenario())],
        ..HarnessConfig::default()
    }
}

#[tokio::test]
async fn agent_select_signs_in_then_new_threads_use_the_agent() {
    let mut harness = Harness::start(two_agents()).await;
    let list = harness
        .request("acpc/agent/list", json!({}))
        .await
        .expect("list");
    assert_eq!(list["current"], "mock");
    assert_eq!(list["agents"].as_array().map(Vec::len), Some(2));

    let select = harness
        .send_request("acpc/agent/select", json!({"slug": "locked"}))
        .await;
    let auth = harness.next("acpc/auth/select").await;
    assert_eq!(auth["params"]["agentName"], "Locked");
    assert_eq!(auth["params"]["methods"][0]["id"], "login");
    harness
        .respond(auth["id"].clone(), json!({"methodId": "login"}))
        .await;
    harness.response(select).await.expect("agent/select");
    // Earlier updates (sent after initialize) still name the old agent.
    loop {
        let updated = harness.next("acpc/agents/updated").await;
        if updated["params"]["current"] == "locked" {
            break;
        }
    }

    let started = harness
        .request("thread/start", json!({}))
        .await
        .expect("thread/start");
    assert_eq!(started["thread"]["modelProvider"], "locked");
    let thread_id = started["thread"]["id"].as_str().expect("id").to_string();
    harness.send_turn(&thread_id, "hello").await;
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "completed");
}

#[tokio::test]
async fn cancelled_sign_in_keeps_the_current_agent() {
    let mut harness = Harness::start(two_agents()).await;
    let select = harness
        .send_request("acpc/agent/select", json!({"slug": "locked"}))
        .await;
    let auth = harness.next("acpc/auth/select").await;
    harness
        .respond(auth["id"].clone(), json!({"methodId": null}))
        .await;
    let error = harness
        .response(select)
        .await
        .expect_err("sign-in was cancelled");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("cancelled")),
        "{error}"
    );
    let list = harness
        .request("acpc/agent/list", json!({}))
        .await
        .expect("list");
    assert_eq!(list["current"], "mock");
}

#[tokio::test]
async fn session_options_and_commands_reach_the_client() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    let config = harness.next("acpc/sessionConfig/updated").await;
    assert_eq!(config["params"]["threadId"], thread_id.as_str());
    // The first update (right after thread/start) may predate the agent's
    // available_commands_update.
    loop {
        let commands = harness.next("acpc/availableCommands/updated").await;
        assert_eq!(commands["params"]["threadId"], thread_id.as_str());
        if commands["params"]["commands"]
            .as_array()
            .is_some_and(|commands| !commands.is_empty())
        {
            break;
        }
    }
}

/// A second agent whose "sign in" turn asks a request-scoped question
/// (as a sign-in or setup step would) after a pause.
fn with_setup_agent() -> HarnessConfig {
    let scenario = json!({
        "turns": [{
            "match": "sign in",
            "steps": [
                {"sleep_ms": 2500},
                {
                    "elicitation": {
                        "requestId": "setup",
                        "mode": "form",
                        "message": "Token?",
                        "requestedSchema": {"type": "object", "properties": {"token": {"type": "string"}}}
                    },
                    "on": {"accept": [{"text": "token {{elicitation.token}}"}]}
                }
            ]
        }]
    });
    HarnessConfig {
        agents: vec![
            ("mock".to_string(), "Mock".to_string()),
            ("setup".to_string(), "Setup".to_string()),
        ],
        agent_scenarios: vec![("setup".to_string(), scenario)],
        ..HarnessConfig::default()
    }
}

/// Thread times have one-second resolution: wait long enough that a thread
/// touched afterwards is the most recent one, which request-scoped
/// questions are shown on.
async fn next_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

#[tokio::test]
async fn turn_end_keeps_another_agents_request_scoped_elicitation() {
    let mut harness = Harness::start(with_setup_agent()).await;
    let setup = harness.start_thread_on("setup").await;
    harness.send_turn(&setup, "sign in").await;
    next_second().await;
    // The question arrives while this thread runs a turn, and is shown on it.
    let mine = harness.start_thread().await;
    let turn = harness.send_turn(&mine, "slow").await;
    let turn_id = turn["turn"]["id"].as_str().expect("turn id").to_string();
    let request = harness.next("mcpServer/elicitation/request").await;

    harness
        .request(
            "turn/interrupt",
            json!({"threadId": mine, "turnId": turn_id}),
        )
        .await
        .expect("turn/interrupt");
    loop {
        let message = harness
            .next_any(&["serverRequest/resolved", "turn/completed"])
            .await;
        assert_ne!(
            message["params"]["requestId"], request["id"],
            "withdrawn: {message}"
        );
        if message["method"] == "turn/completed" && message["params"]["threadId"] == mine.as_str() {
            break;
        }
    }

    // The other agent still gets the answer.
    harness
        .respond(
            request["id"].clone(),
            json!({"action": "accept", "content": {"token": "abc"}, "_meta": null}),
        )
        .await;
    let (items, completed) = completed_items(&mut harness).await;
    assert_eq!(completed["params"]["threadId"], setup.as_str());
    assert_eq!(completed["params"]["turn"]["status"], "completed");
    let text: String = items
        .iter()
        .filter(|item| item["type"] == "agentMessage")
        .filter_map(|item| item["text"].as_str())
        .collect();
    assert!(text.contains("token abc"), "{text}");
}

#[tokio::test]
async fn out_of_turn_elicitation_is_withdrawn_when_the_agent_withdraws_it() {
    let mut harness = Harness::start(with_setup_agent()).await;
    let setup = harness.start_thread_on("setup").await;
    let turn = harness.send_turn(&setup, "sign in").await;
    let turn_id = turn["turn"]["id"].as_str().expect("turn id").to_string();
    next_second().await;
    // Idle: the question is shown on a thread without a turn.
    harness.start_thread().await;
    let request = harness.next("mcpServer/elicitation/request").await;

    // Once its turn is cancelled, the agent withdraws the unanswered
    // question with `$/cancel_request`.
    harness
        .request(
            "turn/interrupt",
            json!({"threadId": setup, "turnId": turn_id}),
        )
        .await
        .expect("turn/interrupt");
    let resolved = harness.next("serverRequest/resolved").await;
    assert_eq!(resolved["params"]["requestId"], request["id"]);
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "interrupted");
}

#[tokio::test]
async fn duplicate_resume_while_the_agent_connects_is_refused() {
    let mut scenario = scenario_with_history();
    scenario["require_auth"] = json!(true);
    scenario["auth_methods"] = json!([
        {"type": "terminal", "id": "tlogin", "name": "Terminal login", "args": ["--login"], "env": {}}
    ]);
    let mut harness = Harness::start(HarnessConfig {
        scenario: Some(scenario),
        terminal_auth: true,
        ..HarnessConfig::default()
    })
    .await;
    let list = harness
        .request("thread/list", json!({}))
        .await
        .expect("thread/list");
    let thread_id = list["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|thread| thread["name"] == "Earlier work")
        .and_then(|thread| thread["id"].as_str())
        .expect("listed session")
        .to_string();

    // Terminal sign-in restarts the agent.
    let select = harness
        .send_request("acpc/agent/select", json!({"slug": "mock"}))
        .await;
    let auth = harness.next("acpc/auth/select").await;
    harness
        .respond(auth["id"].clone(), json!({"methodId": "tlogin"}))
        .await;
    let run = harness.next("acpc/terminal/run").await;
    // What the login command would have recorded.
    std::fs::write(harness.state_dir.path().join("mock/authenticated"), "ok\n")
        .expect("auth marker");
    harness
        .respond(run["id"].clone(), json!({"exitCode": 0}))
        .await;

    // Both wait for the restarted agent.
    let first = harness
        .send_request("thread/resume", json!({"threadId": thread_id}))
        .await;
    let second = harness
        .send_request("thread/resume", json!({"threadId": thread_id}))
        .await;
    let error = harness
        .response(second)
        .await
        .expect_err("duplicate resume");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("already being opened")),
        "{error}"
    );
    let resumed = harness.response(first).await.expect("thread/resume");
    assert_eq!(resumed["thread"]["turns"].as_array().map(Vec::len), Some(2));
    harness.response(select).await.expect("agent/select");
}

#[tokio::test]
async fn only_threads_their_agent_can_reopen_are_resumable() {
    let mut harness = Harness::start(HarnessConfig {
        agents: vec![
            ("mock".to_string(), "Mock".to_string()),
            ("nolist".to_string(), "No list".to_string()),
        ],
        agent_scenarios: vec![(
            "nolist".to_string(),
            json!({"capabilities": {"list": false}}),
        )],
        ..HarnessConfig::default()
    })
    .await;
    let listed = harness.start_thread().await;
    let unlisted = harness.start_thread_on("nolist").await;
    let resumable = harness.resumable.lock().expect("resumable").clone();
    assert_eq!(resumable.get(&listed).map(String::as_str), Some("mock"));
    assert!(!resumable.contains_key(&unlisted));
}

#[tokio::test]
async fn hang_up_lets_the_running_turn_finish_then_stops() {
    let mut harness = Harness::start(HarnessConfig::default()).await;
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "slow").await;
    harness.hang_up().await;
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "completed");
    harness.wait_closed().await;
}

#[tokio::test]
async fn prompt_that_needs_sign_in_offers_it_then_works() {
    let mut harness = Harness::start(HarnessConfig {
        scenario: Some(json!({
            "require_auth_for_prompts": true,
            "auth_methods": [{"id": "login", "name": "Log in"}]
        })),
        ..HarnessConfig::default()
    })
    .await;
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "hello").await;
    let failed = harness.next("turn/completed").await;
    assert_eq!(failed["params"]["turn"]["status"], "failed");
    let auth = harness.next("acpc/auth/select").await;
    assert_eq!(auth["params"]["methods"][0]["id"], "login");
    harness
        .respond(auth["id"].clone(), json!({"methodId": "login"}))
        .await;
    let notice = harness.next("acpc/notice").await;
    assert_eq!(notice["params"]["threadId"], thread_id.as_str());
    assert_eq!(
        notice["params"]["title"],
        "Signed in. Send your message again."
    );

    harness.send_turn(&thread_id, "hello").await;
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "completed");
}

#[tokio::test]
async fn switching_agents_can_bring_the_conversation() {
    let mut harness = Harness::start(HarnessConfig {
        agents: vec![
            ("mock".to_string(), "Mock".to_string()),
            ("other".to_string(), "Other".to_string()),
        ],
        ..HarnessConfig::default()
    })
    .await;
    let first = harness.start_thread().await;
    harness
        .send_turn(&first, "remember the word pineapple")
        .await;
    harness.next("turn/completed").await;

    // `/agent` → "Bring this conversation": select, then fork onto it.
    harness
        .request("acpc/agent/select", json!({"slug": "other"}))
        .await
        .expect("agent/select");
    let forked = harness
        .request("thread/fork", json!({"threadId": first}))
        .await
        .expect("thread/fork");
    assert_eq!(forked["modelProvider"], "other");
    assert_eq!(forked["thread"]["forkedFromId"], first.as_str());
    assert_eq!(
        forked["thread"]["turns"].as_array().map(Vec::len),
        Some(1),
        "{forked}"
    );
    let second = forked["thread"]["id"].as_str().expect("id").to_string();
    assert_ne!(second, first);

    harness.send_turn(&second, "what was the word?").await;
    let completed = harness.next("turn/completed").await;
    assert_eq!(completed["params"]["turn"]["status"], "completed");

    // The fork shows the source's turn, then only what the user typed.
    let read = harness
        .request(
            "thread/read",
            json!({"threadId": second, "includeTurns": true}),
        )
        .await
        .expect("thread/read");
    let turns = read["thread"]["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 2, "{read}");
    let user = &turns[1]["items"][0];
    assert_eq!(user["content"].as_array().map(Vec::len), Some(1), "{user}");
    assert_eq!(user["content"][0]["text"], "what was the word?");
}

/// What claude-agent-acp 0.106 sends a v1 client for a Write that creates a
/// file (no permission asked), a Write that overwrites one and an Edit (both
/// asked): the call opens with no diff, the diff follows from the input, and
/// once the tool ran a hook sends the final diff, one block per hunk.
fn claude_edit_turn() -> Value {
    let call = |id: &str, name: &str| {
        json!({"update": {"sessionUpdate": "tool_call", "toolCallId": id, "name": name,
            "status": "pending", "title": if name == "Edit" { "Edit" } else { "Preparing file…" },
            "kind": "edit", "content": [], "locations": []}})
    };
    let update = |id: &str, fields: Value| {
        let mut update = json!({"sessionUpdate": "tool_call_update", "toolCallId": id});
        update
            .as_object_mut()
            .expect("object")
            .extend(fields.as_object().expect("fields").clone());
        json!({"update": update})
    };
    let diff = |file: &str, old: Value, new: &str| json!([{"type": "diff", "path": format!("{{{{cwd}}}}/{file}"), "oldText": old, "newText": new}]);
    let at = |file: &str, line: Option<u32>| match line {
        Some(line) => json!([{"path": format!("{{{{cwd}}}}/{file}"), "line": line}]),
        None => json!([{"path": format!("{{{{cwd}}}}/{file}")}]),
    };
    let completed = |id: &str| update(id, json!({"status": "completed"}));
    let permission = |id: &str, title: &str, file: &str, content: Value, on: Vec<Value>| {
        json!({"permission": {
            "tool_call": {"toolCallId": id, "title": title, "kind": "edit", "status": "pending",
                "content": content, "locations": at(file, None)},
            "on": {"allow_once": on}
        }})
    };
    json!({"turns": [{"match": "claude edits", "steps": [
        call("w1", "Write"),
        update("w1", json!({"title": "Write new.txt", "locations": at("new.txt", None)})),
        update("w1", json!({"content": diff("new.txt", Value::Null, "one\ntwo\nthree\n")})),
        update("w1", json!({"_meta": {"claudeCode": {"toolName": "Write"}}})),
        completed("w1"),
        call("w2", "Write"),
        update("w2", json!({"title": "Write old.txt", "locations": at("old.txt", None)})),
        update("w2", json!({"content": diff("old.txt", Value::Null, "a\nb\nX\nd\ne\n")})),
        permission("w2", "Write old.txt", "old.txt", diff("old.txt", Value::Null, "a\nb\nX\nd\ne\n"), vec![
            update("w2", json!({"content": diff("old.txt", json!("a\nb\nc\nd\ne"), "a\nb\nX\nd\ne"),
                "locations": at("old.txt", Some(1))})),
            completed("w2"),
        ]),
        call("e1", "Edit"),
        update("e1", json!({"title": "Edit edit.txt", "locations": at("edit.txt", None)})),
        update("e1", json!({})),
        update("e1", json!({"content": diff("edit.txt", json!("beta"), "BETA")})),
        permission("e1", "Edit edit.txt", "edit.txt", diff("edit.txt", json!("beta"), "BETA"), vec![
            completed("e1"),
            // The hook may land after the result.
            update("e1", json!({"content": diff("edit.txt", json!("beta\ngamma"), "BETA\ngamma"),
                "locations": at("edit.txt", Some(2))})),
        ]),
        {"text": "done"}
    ]}]})
}

/// The `+N -M` the TUI draws for a change.
fn change_counts(change: &Value) -> (usize, usize) {
    let diff = change["diff"].as_str().expect("diff");
    match change["kind"]["type"].as_str() {
        Some("add") => (diff.lines().count(), 0),
        _ => (
            diff.lines().filter(|line| line.starts_with('+')).count(),
            diff.lines().filter(|line| line.starts_with('-')).count(),
        ),
    }
}

#[tokio::test]
async fn claude_edits_start_with_their_final_diff() {
    let mut harness = Harness::start(HarnessConfig {
        scenario: Some(claude_edit_turn()),
        ..HarnessConfig::default()
    })
    .await;
    let cwd = harness.cwd.path().to_path_buf();
    std::fs::write(cwd.join("old.txt"), "a\nb\nc\nd\ne\n").expect("write");
    std::fs::write(cwd.join("edit.txt"), "alpha\nbeta\ngamma\n").expect("write");
    let thread_id = harness.start_thread().await;
    harness.send_turn(&thread_id, "claude edits").await;

    let mut started = Vec::new();
    let mut completed = Vec::new();
    loop {
        let message = harness
            .next_any(&[
                "item/started",
                "item/completed",
                "item/fileChange/requestApproval",
                "turn/completed",
            ])
            .await;
        match message["method"].as_str() {
            Some("turn/completed") => break,
            Some("item/fileChange/requestApproval") => {
                let item = message["params"]["itemId"].clone();
                assert!(
                    started.iter().any(|started: &Value| started["id"] == item),
                    "the change is shown before its approval"
                );
                harness
                    .respond(message["id"].clone(), json!({"decision": "accept"}))
                    .await;
            }
            Some("item/started") if item_type(&message) == "fileChange" => {
                started.push(message["params"]["item"].clone());
            }
            Some("item/completed") if item_type(&message) == "fileChange" => {
                completed.push(message["params"]["item"].clone());
            }
            _ => {}
        }
    }
    assert_eq!(started.len(), 3, "{started:#?}");
    let created = &started[0]["changes"][0];
    assert_eq!(created["kind"]["type"], "add");
    assert_eq!(created["diff"], "one\ntwo\nthree\n");
    // The overwrite is read against the file before it is written.
    let overwrite = &started[1]["changes"][0];
    assert_eq!(overwrite["kind"]["type"], "update");
    assert_eq!(
        overwrite["diff"],
        "@@ -1,5 +1,5 @@\n a\n b\n-c\n+X\n d\n e\n"
    );
    let edit = &started[2]["changes"][0];
    assert_eq!(
        edit["diff"],
        "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n"
    );

    assert_eq!(completed.len(), 3);
    let counts: Vec<_> = completed
        .iter()
        .map(|item| {
            assert_eq!(item["status"], "completed");
            assert_eq!(item["changes"].as_array().map(Vec::len), Some(1));
            change_counts(&item["changes"][0])
        })
        .collect();
    assert_eq!(counts, vec![(3, 0), (1, 1), (1, 1)]);
    assert_eq!(
        completed[1]["changes"][0]["diff"],
        "@@ -1,5 +1,5 @@\n a\n b\n-c\n+X\n d\n e\n"
    );

    // The late hook diff reaches the stored turn, at its line.
    let read = harness
        .request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )
        .await
        .expect("thread/read");
    let items = read["thread"]["turns"][0]["items"]
        .as_array()
        .expect("items");
    let edit = items
        .iter()
        .filter(|item| item["type"] == "fileChange")
        .nth(2)
        .expect("edit item");
    assert_eq!(
        edit["changes"][0]["diff"],
        "@@ -2,2 +2,2 @@\n-beta\n+BETA\n gamma\n"
    );
}

/// What claude-agent-acp replays for those calls on `session/load`: the
/// diffs from the input, never the hook's.
fn claude_edit_history() -> Value {
    let call = |id: &str, title: &str, file: &str, old: Value, new: &str| {
        json!({"sessionUpdate": "tool_call", "toolCallId": id, "status": "pending", "title": title,
            "kind": "edit",
            "content": [{"type": "diff", "path": format!("/w/{file}"), "oldText": old, "newText": new}],
            "locations": [{"path": format!("/w/{file}")}]})
    };
    let completed = |id: &str| json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": "completed"});
    json!({"sessions": [{"session_id": "claude-1", "cwd": "/", "title": "Claude edits",
    "updated_at": "2026-01-01T00:00:00Z",
    "history": [
        {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "edit"}},
        call("w1", "Write new.txt", "new.txt", Value::Null, "one\ntwo\nthree\n"),
        completed("w1"),
        call("e1", "Edit edit.txt", "edit.txt", json!("beta"), "BETA"),
        completed("e1"),
        {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "done"}}
    ]}]})
}

#[tokio::test]
async fn resumed_claude_edits_keep_their_diffs() {
    let mut harness = Harness::start(HarnessConfig {
        scenario: Some(claude_edit_history()),
        ..HarnessConfig::default()
    })
    .await;
    let list = harness
        .request("thread/list", json!({}))
        .await
        .expect("thread/list");
    let thread_id = list["data"]
        .as_array()
        .expect("data")
        .iter()
        .find(|thread| thread["name"] == "Claude edits")
        .and_then(|thread| thread["id"].as_str())
        .expect("listed session")
        .to_string();
    let resumed = harness
        .request("thread/resume", json!({"threadId": thread_id}))
        .await
        .expect("thread/resume");
    let changes: Vec<Value> = resumed["thread"]["turns"][0]["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|item| item["type"] == "fileChange")
        .map(|item| {
            assert_eq!(item["status"], "completed");
            item["changes"][0].clone()
        })
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0]["diff"], "one\ntwo\nthree\n");
    assert_eq!(change_counts(&changes[0]), (3, 0));
    assert_eq!(changes[1]["diff"], "@@ -1 +1 @@\n-beta\n+BETA\n");
    assert_eq!(change_counts(&changes[1]), (1, 1));
}
