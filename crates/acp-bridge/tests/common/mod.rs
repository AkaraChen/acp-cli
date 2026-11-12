//! Drives the bridge in-process against the `acpc-mock-agent` binary.
//!
//! Build the mock agent first (`cargo build -p acpc-mock-agent`); the
//! harness finds it next to the test executable's target directory.

#![allow(clippy::expect_used, dead_code)]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use acp_bridge::AgentProfile;
use acp_bridge::BridgeOptions;
use acp_bridge::BridgeTransport;
use acp_client::AgentCommand;
use acp_client::ClientOptions;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCRequest;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_transport::ConnectionId;
use codex_app_server_transport::ConnectionOrigin;
use codex_app_server_transport::QueuedOutgoingMessage;
use codex_app_server_transport::TransportEvent;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const TIMEOUT: Duration = Duration::from_secs(20);

pub fn mock_agent_path() -> PathBuf {
    let exe = std::env::current_exe().expect("test exe");
    // target/<profile>/deps/<test> → target/<profile>/acpc-mock-agent
    let dir = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target dir");
    let path = dir.join("acpc-mock-agent");
    assert!(
        path.exists(),
        "{} not found; run `cargo build -p acpc-mock-agent` first",
        path.display()
    );
    path
}

pub struct Harness {
    tx: mpsc::Sender<TransportEvent>,
    rx: mpsc::Receiver<QueuedOutgoingMessage>,
    pending: VecDeque<Value>,
    next_id: i64,
    methods: Vec<(i64, String)>,
    shutdown: CancellationToken,
    pub state_dir: tempfile::TempDir,
    pub cwd: tempfile::TempDir,
    pub resumable: Arc<Mutex<HashMap<String, String>>>,
}

pub struct HarnessConfig {
    /// Scenario for every agent without its own.
    pub scenario: Option<Value>,
    pub agents: Vec<(String, String)>,
    /// Per-agent scenarios, by slug.
    pub agent_scenarios: Vec<(String, Value)>,
    /// The client runs terminal sign-in (`acpc/terminal/run`).
    pub terminal_auth: bool,
    /// The bridge approves permission requests itself (`--yolo`).
    pub yolo: bool,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            scenario: None,
            agents: vec![("mock".to_string(), "Mock".to_string())],
            agent_scenarios: Vec::new(),
            terminal_auth: false,
            yolo: false,
        }
    }
}

impl Harness {
    pub async fn start(config: HarnessConfig) -> Self {
        let state_dir = tempfile::tempdir().expect("state dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let agents = config
            .agents
            .iter()
            .map(|(slug, name)| {
                // Each agent keeps its own state (sessions, sign-in marker).
                let agent_dir = state_dir.path().join(slug);
                std::fs::create_dir_all(&agent_dir).expect("agent state dir");
                let mut env = vec![(
                    "ACPC_MOCK_STATE".to_string(),
                    agent_dir.display().to_string(),
                )];
                let scenario = config
                    .agent_scenarios
                    .iter()
                    .find(|(scenario_slug, _)| scenario_slug == slug)
                    .map(|(_, scenario)| scenario)
                    .or(config.scenario.as_ref());
                if let Some(scenario) = scenario {
                    let path = agent_dir.join("scenario.json");
                    std::fs::write(&path, serde_json::to_vec(scenario).expect("json"))
                        .expect("write");
                    env.push(("ACPC_MOCK_SCENARIO".to_string(), path.display().to_string()));
                }
                AgentProfile {
                    slug: slug.clone(),
                    name: name.clone(),
                    command: AgentCommand {
                        command: mock_agent_path().display().to_string(),
                        args: Vec::new(),
                        env,
                        env_remove: Vec::new(),
                        cwd: Some(cwd.path().to_path_buf()),
                    },
                }
            })
            .collect();
        let options = BridgeOptions {
            agents,
            default_agent: config.agents[0].0.clone(),
            codex_home: AbsolutePathBuf::from_absolute_path(state_dir.path()).expect("abs"),
            default_cwd: AbsolutePathBuf::from_absolute_path(cwd.path()).expect("abs"),
            client: ClientOptions {
                name: "acpc-test".to_string(),
                title: None,
                version: "0.0.0".to_string(),
                terminal_auth: config.terminal_auth,
            },
            mcp_servers: Vec::new(),
            user_agent: "acpc/0.0.0".to_string(),
            resumable: Arc::default(),
            yolo: config.yolo,
        };
        let resumable = Arc::clone(&options.resumable);
        let (tx, events) = mpsc::channel(128);
        let shutdown = CancellationToken::new();
        acp_bridge::start(
            options,
            BridgeTransport::Channel(events),
            None,
            shutdown.clone(),
        )
        .await
        .expect("bridge");
        let (writer, rx) = mpsc::channel(1024);
        tx.send(TransportEvent::ConnectionOpened {
            connection_id: ConnectionId(1),
            origin: ConnectionOrigin::InProcess,
            auth: None,
            writer,
            disconnect_sender: None,
        })
        .await
        .expect("open");
        let mut harness = Self {
            tx,
            rx,
            pending: VecDeque::new(),
            next_id: 1,
            methods: Vec::new(),
            shutdown,
            state_dir,
            cwd,
            resumable,
        };
        harness
            .request(
                "initialize",
                json!({"clientInfo": {"name": "test", "version": "0"}}),
            )
            .await
            .expect("initialize");
        harness.notify("initialized", None).await;
        harness
    }

    async fn send(&self, message: JSONRPCMessage) {
        self.tx
            .send(TransportEvent::IncomingMessage {
                connection_id: ConnectionId(1),
                message,
            })
            .await
            .expect("send");
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) {
        self.send(JSONRPCMessage::Notification(JSONRPCNotification {
            method: method.to_string(),
            params,
        }))
        .await;
    }

    async fn recv(&mut self) -> Value {
        let message = tokio::time::timeout(TIMEOUT, self.rx.recv())
            .await
            .expect("timed out waiting for the bridge")
            .expect("bridge closed");
        let value = serde_json::to_value(&message.message).expect("json");
        assert_client_can_decode(&value);
        value
    }

    /// Send a request without waiting; pair with [`Self::response`].
    pub async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.methods.push((id, method.to_string()));
        self.send(JSONRPCMessage::Request(JSONRPCRequest {
            id: RequestId::Integer(id),
            method: method.to_string(),
            params: Some(params),
            trace: None,
        }))
        .await;
        id
    }

    /// The result (or error object) for request `id`. Other messages
    /// received meanwhile (other responses included) are kept for later.
    pub async fn response(&mut self, id: i64) -> Result<Value, Value> {
        let method = self
            .methods
            .iter()
            .find(|(sent, _)| *sent == id)
            .map(|(_, method)| method.clone())
            .unwrap_or_default();
        let is_response = |message: &Value| {
            message.get("method").is_none() && message.get("id") == Some(&json!(id))
        };
        let message = match self.pending.iter().position(is_response) {
            Some(index) => self.pending.remove(index).expect("index"),
            None => loop {
                let message = self.recv().await;
                if is_response(&message) {
                    break message;
                }
                self.pending.push_back(message);
            },
        };
        if let Some(result) = message.get("result") {
            assert_response_decodes(&method, id, result);
            return Ok(result.clone());
        }
        Err(message.get("error").cloned().unwrap_or(Value::Null))
    }

    /// Send a request and return its result (or error object).
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send_request(method, params).await;
        self.response(id).await
    }

    /// Next notification or server request whose method matches.
    pub async fn next(&mut self, method: &str) -> Value {
        if let Some(index) = self
            .pending
            .iter()
            .position(|message| message.get("method").and_then(Value::as_str) == Some(method))
        {
            return self.pending.remove(index).expect("index");
        }
        loop {
            let message = self.recv().await;
            if message.get("method").and_then(Value::as_str) == Some(method) {
                return message;
            }
            self.pending.push_back(message);
        }
    }

    /// Next message whose method is any of `methods`, in arrival order.
    pub async fn next_any(&mut self, methods: &[&str]) -> Value {
        let matches = |message: &Value| {
            message
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|method| methods.contains(&method))
        };
        if let Some(index) = self.pending.iter().position(matches) {
            return self.pending.remove(index).expect("index");
        }
        loop {
            let message = self.recv().await;
            if matches(&message) {
                return message;
            }
            self.pending.push_back(message);
        }
    }

    /// Every method received until (and including) `method`.
    pub async fn methods_until(&mut self, method: &str) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        while let Some(message) = self.pending.pop_front() {
            let name = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            seen.push(name.clone());
            if name == method {
                return seen;
            }
        }
        loop {
            let message = self.recv().await;
            let name = message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            seen.push(name.clone());
            if name == method {
                return seen;
            }
        }
    }

    pub async fn respond(&self, id: Value, result: Value) {
        let id: RequestId = serde_json::from_value(id).expect("request id");
        self.send(JSONRPCMessage::Response(JSONRPCResponse { id, result }))
            .await;
    }

    pub async fn start_thread(&mut self) -> String {
        let result = self
            .request("thread/start", json!({}))
            .await
            .expect("thread/start");
        result["thread"]["id"]
            .as_str()
            .expect("thread id")
            .to_string()
    }

    /// Start a thread on `agent` instead of the current one.
    pub async fn start_thread_on(&mut self, agent: &str) -> String {
        let result = self
            .request("thread/start", json!({"config": {"acpc.agent": agent}}))
            .await
            .expect("thread/start");
        result["thread"]["id"]
            .as_str()
            .expect("thread id")
            .to_string()
    }

    /// Close the connection, as a stdio client does by closing stdin.
    pub async fn hang_up(&mut self) {
        self.tx
            .send(TransportEvent::ConnectionClosed {
                connection_id: ConnectionId(1),
            })
            .await
            .expect("hang up");
    }

    /// Wait until the bridge has written everything and let go of the
    /// connection.
    pub async fn wait_closed(&mut self) {
        while tokio::time::timeout(TIMEOUT, self.rx.recv())
            .await
            .expect("bridge did not stop")
            .is_some()
        {}
    }

    pub async fn send_turn(&mut self, thread_id: &str, text: &str) -> Value {
        self.request(
            "turn/start",
            json!({"threadId": thread_id, "input": [{"type": "text", "text": text}]}),
        )
        .await
        .expect("turn/start")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Contract: every notification and server request the bridge emits must
/// decode as the typed messages the Codex TUI parses. The remote client
/// silently drops what it cannot decode, so drift would otherwise go unseen.
pub fn assert_client_can_decode(message: &Value) {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return;
    };
    if message.get("id").is_some() {
        let request: JSONRPCRequest = serde_json::from_value(message.clone())
            .unwrap_or_else(|err| panic!("server request {method} is not JSON-RPC: {err}"));
        codex_app_server_protocol::ServerRequest::try_from(request)
            .unwrap_or_else(|err| panic!("TUI cannot decode server request {method}: {err}"));
    } else {
        let notification: JSONRPCNotification = serde_json::from_value(message.clone())
            .unwrap_or_else(|err| panic!("notification {method} is not JSON-RPC: {err}"));
        codex_app_server_protocol::ServerNotification::try_from(notification)
            .unwrap_or_else(|err| panic!("TUI cannot decode notification {method}: {err}"));
    }
}

/// Contract: a response must decode as the typed response of its method.
pub fn assert_response_decodes(method: &str, id: i64, result: &Value) {
    let tagged = json!({ "method": method, "id": id, "response": result });
    serde_json::from_value::<codex_app_server_protocol::ClientResponse>(tagged)
        .unwrap_or_else(|err| panic!("TUI cannot decode the {method} response: {err}\n{result}"));
}
