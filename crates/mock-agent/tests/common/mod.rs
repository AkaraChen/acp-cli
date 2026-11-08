//! Spawns the built `acpc-mock-agent` and drives it with the SDK's client
//! role over the child's stdio.

#![allow(dead_code)]
#![allow(clippy::expect_used, reason = "test harness: failures should panic")]

use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use agent_client_protocol::Agent;
use agent_client_protocol::ByteStreams;
use agent_client_protocol::Client;
use agent_client_protocol::Error;
use agent_client_protocol::JsonRpcRequest;
use agent_client_protocol::on_receive_notification;
use agent_client_protocol::on_receive_request;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::Responder;
use agent_client_protocol::v1::schema as acp;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

/// Upper bound for a whole client session against the agent.
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound for a single expected message.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(5);

pub type PendingPermission = (
    acp::RequestPermissionRequest,
    Responder<acp::RequestPermissionResponse>,
);

pub type PendingElicitation = (
    acp::CreateElicitationRequest,
    Responder<acp::CreateElicitationResponse>,
);

/// One mock agent configuration with its own state and working directories.
pub struct MockAgent {
    pub state: tempfile::TempDir,
    pub cwd: tempfile::TempDir,
    scenario: Option<PathBuf>,
}

impl MockAgent {
    pub fn new(scenario: Option<Value>) -> Self {
        let state = tempfile::tempdir().expect("state dir");
        let cwd = tempfile::tempdir().expect("cwd");
        let scenario = scenario.map(|scenario| {
            let path = cwd.path().join("scenario.json");
            std::fs::write(&path, serde_json::to_vec(&scenario).expect("json")).expect("write");
            path
        });
        Self {
            state,
            cwd,
            scenario,
        }
    }

    pub fn cwd(&self) -> &Path {
        self.cwd.path()
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_acpc-mock-agent"));
        command
            .env("ACPC_MOCK_STATE", self.state.path())
            .env_remove("ACPC_MOCK_SCENARIO")
            .env_remove("ACPC_MOCK_TRACE")
            .current_dir(self.cwd.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        if std::env::var_os("ACPC_MOCK_TEST_STDERR").is_none() {
            command.stderr(Stdio::null());
        }
        if let Some(path) = &self.scenario {
            command.arg("--scenario").arg(path);
        }
        command
    }

    /// Runs `body` against a fresh agent process; panics on timeout.
    pub async fn connect<R>(&self, body: impl AsyncFnOnce(Conn) -> Result<R, Error>) -> R {
        let mut child = self.command().spawn().expect("spawn mock agent");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (update_tx, updates) = mpsc::unbounded_channel();
        let (permission_tx, permissions) = mpsc::unbounded_channel();
        let (elicitation_tx, elicitations) = mpsc::unbounded_channel();
        let (complete_tx, completions) = mpsc::unbounded_channel();
        let run = Client
            .v1()
            .name("mock-agent-e2e")
            .on_receive_notification(
                async move |notification: acp::SessionNotification, _cx| {
                    update_tx.send(notification).ok();
                    Ok(())
                },
                on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: acp::RequestPermissionRequest, responder, _cx| {
                    permission_tx.send((request, responder)).ok();
                    Ok(())
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |request: acp::CreateElicitationRequest, responder, _cx| {
                    elicitation_tx.send((request, responder)).ok();
                    Ok(())
                },
                on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: acp::CompleteElicitationNotification, _cx| {
                    complete_tx.send(notification).ok();
                    Ok(())
                },
                on_receive_notification!(),
            )
            .connect_with(
                ByteStreams::new(stdin.compat_write(), stdout.compat()),
                async move |cx| {
                    body(Conn {
                        cx,
                        updates,
                        permissions,
                        elicitations,
                        completions,
                    })
                    .await
                },
            );
        let result = tokio::time::timeout(TIMEOUT, run)
            .await
            .expect("test timed out")
            .expect("client connection failed");
        drop(child);
        result
    }
}

/// The client side of one connection.
pub struct Conn {
    pub cx: ConnectionTo<Agent>,
    updates: mpsc::UnboundedReceiver<acp::SessionNotification>,
    permissions: mpsc::UnboundedReceiver<PendingPermission>,
    elicitations: mpsc::UnboundedReceiver<PendingElicitation>,
    completions: mpsc::UnboundedReceiver<acp::CompleteElicitationNotification>,
}

impl Conn {
    pub async fn request<Req: JsonRpcRequest>(&self, request: Req) -> Result<Req::Response, Error> {
        self.cx.send_request(request).block_task().await
    }

    pub async fn initialize(&self, caps: acp::ClientCapabilities) -> acp::InitializeResponse {
        self.request(acp::InitializeRequest::new(ProtocolVersion::V1).client_capabilities(caps))
            .await
            .expect("initialize")
    }

    /// Initializes with default capabilities and opens a session in `cwd`.
    pub async fn start(&mut self, cwd: &Path) -> acp::SessionId {
        self.initialize(acp::ClientCapabilities::new()).await;
        let session = self
            .request(acp::NewSessionRequest::new(cwd))
            .await
            .expect("session/new");
        session.session_id
    }

    pub async fn prompt(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> Result<acp::PromptResponse, Error> {
        self.send_prompt(session_id, text).await
    }

    /// Sends `session/prompt` now; the returned future yields its response.
    pub fn send_prompt(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> impl Future<Output = Result<acp::PromptResponse, Error>> + use<> {
        self.cx
            .send_request(acp::PromptRequest::new(
                session_id.clone(),
                vec![text.into()],
            ))
            .block_task()
    }

    pub async fn next_update(&mut self) -> acp::SessionUpdate {
        let notification = tokio::time::timeout(STEP_TIMEOUT, self.updates.recv())
            .await
            .expect("timed out waiting for session/update")
            .expect("connection closed");
        notification.update
    }

    /// Waits for the first update matching `pred`, discarding earlier ones.
    pub async fn wait_for(
        &mut self,
        pred: impl Fn(&acp::SessionUpdate) -> bool,
    ) -> acp::SessionUpdate {
        loop {
            let update = self.next_update().await;
            if pred(&update) {
                return update;
            }
        }
    }

    /// Collects updates up to and including the first matching `pred`.
    pub async fn collect_until(
        &mut self,
        pred: impl Fn(&acp::SessionUpdate) -> bool,
    ) -> Vec<acp::SessionUpdate> {
        let mut updates = Vec::new();
        loop {
            let update = self.next_update().await;
            let done = pred(&update);
            updates.push(update);
            if done {
                return updates;
            }
        }
    }

    /// The updates before the next `available_commands_update`, which the
    /// agent sends right after answering new/load/resume.
    pub async fn until_commands(&mut self) -> Vec<acp::SessionUpdate> {
        let mut updates = self
            .collect_until(|update| {
                matches!(update, acp::SessionUpdate::AvailableCommandsUpdate(_))
            })
            .await;
        updates.pop();
        updates
    }

    /// Every update received so far.
    pub fn drain(&mut self) -> Vec<acp::SessionUpdate> {
        let mut updates = Vec::new();
        while let Ok(notification) = self.updates.try_recv() {
            updates.push(notification.update);
        }
        updates
    }

    pub async fn next_permission(&mut self) -> PendingPermission {
        tokio::time::timeout(STEP_TIMEOUT, self.permissions.recv())
            .await
            .expect("timed out waiting for session/request_permission")
            .expect("connection closed")
    }

    pub async fn next_elicitation(&mut self) -> PendingElicitation {
        tokio::time::timeout(STEP_TIMEOUT, self.elicitations.recv())
            .await
            .expect("timed out waiting for elicitation/create")
            .expect("connection closed")
    }

    pub async fn next_completion(&mut self) -> acp::CompleteElicitationNotification {
        tokio::time::timeout(STEP_TIMEOUT, self.completions.recv())
            .await
            .expect("timed out waiting for elicitation/complete")
            .expect("connection closed")
    }

    /// Whether any permission or elicitation request arrived unasked-for.
    pub fn has_pending_requests(&mut self) -> bool {
        !self.permissions.is_empty() || !self.elicitations.is_empty()
    }
}

/// The response, if `future` is already complete.
pub async fn ready<T>(future: impl Future<Output = T>) -> Option<T> {
    tokio::time::timeout(Duration::ZERO, future).await.ok()
}

/// Concatenated text of the `agent_message_chunk`s in `updates`.
pub fn message_text(updates: &[acp::SessionUpdate]) -> String {
    updates
        .iter()
        .filter_map(|update| match update {
            acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

pub fn is_message_chunk(update: &acp::SessionUpdate) -> bool {
    matches!(update, acp::SessionUpdate::AgentMessageChunk(_))
}

pub fn is_tool_status(
    id: &str,
    status: acp::ToolCallStatus,
) -> impl Fn(&acp::SessionUpdate) -> bool {
    move |update| {
        matches!(update, acp::SessionUpdate::ToolCallUpdate(call)
            if call.tool_call_id.to_string() == id && call.fields.status == Some(status))
    }
}

/// The `(id, status)` of every tool call and tool call update in `updates`.
pub fn tool_statuses(updates: &[acp::SessionUpdate]) -> Vec<(String, Option<acp::ToolCallStatus>)> {
    updates
        .iter()
        .filter_map(|update| match update {
            acp::SessionUpdate::ToolCall(call) => {
                Some((call.tool_call_id.to_string(), Some(call.status)))
            }
            acp::SessionUpdate::ToolCallUpdate(call) => {
                Some((call.tool_call_id.to_string(), call.fields.status))
            }
            _ => None,
        })
        .collect()
}

pub fn select(option_id: &str) -> acp::RequestPermissionResponse {
    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Selected(
        acp::SelectedPermissionOutcome::new(option_id.to_string()),
    ))
}
