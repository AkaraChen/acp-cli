use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_client_protocol::Agent;
use agent_client_protocol::ByteStreams;
use agent_client_protocol::Client;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::Responder;
use agent_client_protocol::v1::schema as acp;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::AcpError;
use crate::AgentCaps;
use crate::AgentCommand;
use crate::ClientOptions;
use crate::ConnectError;
use crate::process::AgentProcess;
use crate::process::StderrTail;
use crate::process::spawn_agent;

/// How long an agent may take to answer `initialize`. Generous: on first
/// run, npx downloads and installs the agent before it reads stdin.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(300);

/// Identifies an agent-initiated request awaiting a user decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PendingRequestId(pub u64);

/// Identifies one `session/prompt` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptId(pub u64);

/// Identifies one `session/load` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LoadId(pub u64);

/// Everything the agent sends that is not a direct response, in wire order.
#[derive(Debug)]
pub enum AgentEvent {
    SessionUpdate(acp::SessionNotification),
    PermissionRequested {
        id: PendingRequestId,
        request: acp::RequestPermissionRequest,
    },
    ElicitationRequested {
        id: PendingRequestId,
        request: acp::CreateElicitationRequest,
    },
    ElicitationCompleted(acp::CompleteElicitationNotification),
    /// A pending request was answered without the user: the agent sent
    /// `$/cancel_request`, or the turn it belonged to was cancelled.
    PendingRequestResolved { id: PendingRequestId },
    PromptFinished {
        session_id: acp::SessionId,
        prompt_id: PromptId,
        result: Result<acp::PromptResponse, AcpError>,
    },
    LoadFinished {
        session_id: acp::SessionId,
        load_id: LoadId,
        result: Result<acp::LoadSessionResponse, AcpError>,
    },
    /// The connection ended. No further events follow.
    Exited { reason: String, stderr: String },
}

enum Pending {
    Permission {
        session_id: acp::SessionId,
        responder: Responder<acp::RequestPermissionResponse>,
        _done: oneshot::Sender<()>,
    },
    Elicitation {
        session_id: Option<acp::SessionId>,
        responder: Responder<acp::CreateElicitationResponse>,
        _done: oneshot::Sender<()>,
    },
}

impl Pending {
    fn session_id(&self) -> Option<&acp::SessionId> {
        match self {
            Pending::Permission { session_id, .. } => Some(session_id),
            Pending::Elicitation { session_id, .. } => session_id.as_ref(),
        }
    }

    /// Answer as the spec requires when the request is abandoned.
    fn resolve_cancelled(self) {
        let result = match self {
            Pending::Permission { responder, .. } => responder.respond(
                acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
            ),
            Pending::Elicitation { responder, .. } => {
                responder.respond(acp::CreateElicitationResponse::new(
                    acp::ElicitationAction::Cancel,
                ))
            }
        };
        if let Err(err) = result {
            tracing::debug!("failed to answer abandoned agent request: {err:?}");
        }
    }
}

struct Shared {
    events: mpsc::UnboundedSender<AgentEvent>,
    pending: Mutex<HashMap<PendingRequestId, Pending>>,
    next_id: AtomicU64,
}

impl Shared {
    fn emit(&self, event: AgentEvent) {
        // The receiver going away only means nobody listens anymore.
        let _ = self.events.send(event);
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<PendingRequestId, Pending>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn take(&self, id: PendingRequestId) -> Option<Pending> {
        self.pending().remove(&id)
    }

    /// Store a pending request and watch for the agent cancelling it.
    fn register(
        self: &Arc<Self>,
        cancellation: agent_client_protocol::RequestCancellation,
        make: impl FnOnce(oneshot::Sender<()>) -> Pending,
    ) -> PendingRequestId {
        let id = PendingRequestId(self.next_id());
        let (done_tx, done_rx) = oneshot::channel();
        self.pending().insert(id, make(done_tx));
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                () = cancellation.cancelled() => {
                    if let Some(pending) = shared.take(id) {
                        match pending {
                            Pending::Permission { responder, .. } => {
                                let _ = responder.respond_with_error(
                                    agent_client_protocol::Error::request_cancelled(),
                                );
                            }
                            Pending::Elicitation { responder, .. } => {
                                let _ = responder.respond_with_error(
                                    agent_client_protocol::Error::request_cancelled(),
                                );
                            }
                        }
                        shared.emit(AgentEvent::PendingRequestResolved { id });
                    }
                }
                _ = done_rx => {}
            }
        });
        id
    }
}

/// A live, initialized connection to one agent process.
pub struct AgentConnection {
    cx: ConnectionTo<Agent>,
    caps: AgentCaps,
    command: AgentCommand,
    shared: Arc<Shared>,
    stderr: StderrTail,
    /// Taken by `shutdown`; dropping it kills the agent's process group.
    process: Mutex<Option<AgentProcess>>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
}

impl AgentConnection {
    /// Spawn the agent and complete `initialize`.
    pub async fn connect(
        command: AgentCommand,
        options: &ClientOptions,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<AgentEvent>), ConnectError> {
        let spawned = spawn_agent(&command).map_err(|source| ConnectError::Spawn {
            command: command.display(),
            source,
        })?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            events: events_tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let transport = ByteStreams::new(spawned.stdin.compat_write(), spawned.stdout.compat());
        let (cx_tx, cx_rx) = oneshot::channel::<ConnectionTo<Agent>>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let stderr = spawned.stderr.clone();
        let name = options.name.clone();

        let driver_shared = Arc::clone(&shared);
        let driver_stderr = stderr.clone();
        tokio::spawn(async move {
            let updates = Arc::clone(&driver_shared);
            let completions = Arc::clone(&driver_shared);
            let permissions = Arc::clone(&driver_shared);
            let elicitations = Arc::clone(&driver_shared);
            let result = Client
                .v1()
                .name(name)
                .on_receive_notification(
                    async move |notification: acp::SessionNotification, _cx| {
                        updates.emit(AgentEvent::SessionUpdate(notification));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_notification(
                    async move |notification: acp::CompleteElicitationNotification, _cx| {
                        completions.emit(AgentEvent::ElicitationCompleted(notification));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .on_receive_request(
                    async move |request: acp::RequestPermissionRequest, responder, _cx| {
                        let session_id = request.session_id.clone();
                        let id = permissions.register(responder.cancellation(), |done| {
                            Pending::Permission {
                                session_id,
                                responder,
                                _done: done,
                            }
                        });
                        permissions.emit(AgentEvent::PermissionRequested { id, request });
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: acp::CreateElicitationRequest, responder, _cx| {
                        let session_id = elicitation_session(&request);
                        let id = elicitations.register(responder.cancellation(), |done| {
                            Pending::Elicitation {
                                session_id,
                                responder,
                                _done: done,
                            }
                        });
                        elicitations.emit(AgentEvent::ElicitationRequested { id, request });
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
                    let _ = cx_tx.send(cx.clone());
                    tokio::select! {
                        () = cx.incoming_closed() => {}
                        _ = shutdown_rx => {}
                    }
                    Ok(())
                })
                .await;
            let reason = match result {
                Ok(()) => "agent connection closed".to_string(),
                Err(err) => format!("agent connection failed: {}", AcpError::from(err)),
            };
            // Requests the agent left open can no longer be answered.
            let abandoned: Vec<_> = driver_shared.pending().drain().collect();
            for (id, pending) in abandoned {
                pending.resolve_cancelled();
                driver_shared.emit(AgentEvent::PendingRequestResolved { id });
            }
            driver_shared.emit(AgentEvent::Exited {
                reason,
                stderr: driver_stderr.snapshot(),
            });
        });

        let mut process = spawned.process;
        let cx = match cx_rx.await {
            Ok(cx) => cx,
            Err(_) => {
                process.terminate(Duration::from_secs(2)).await;
                return Err(ConnectError::Exited {
                    stderr: stderr.snapshot(),
                });
            }
        };
        let request = acp::InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(options.client_capabilities())
            .client_info(options.client_info());
        let answer = tokio::time::timeout(INITIALIZE_TIMEOUT, cx.send_request(request).block_task());
        let init = match answer.await {
            Err(_) => {
                process.terminate(Duration::from_secs(2)).await;
                return Err(ConnectError::Timeout {
                    seconds: INITIALIZE_TIMEOUT.as_secs(),
                    stderr: stderr.snapshot(),
                });
            }
            Ok(Ok(init)) => init,
            Ok(Err(err)) => {
                let exited = agent_client_protocol::is_incoming_transport_closed(&err)
                    || agent_client_protocol::is_connection_terminated(&err);
                process.terminate(Duration::from_secs(2)).await;
                if exited {
                    return Err(ConnectError::Exited {
                        stderr: stderr.snapshot(),
                    });
                }
                return Err(ConnectError::Initialize(AcpError::from(err)));
            }
        };
        if init.protocol_version != ProtocolVersion::V1 {
            process.terminate(Duration::from_secs(2)).await;
            return Err(ConnectError::UnsupportedProtocolVersion(format!(
                "{:?}",
                init.protocol_version
            )));
        }
        let connection = Arc::new(Self {
            cx,
            caps: AgentCaps { init },
            command,
            shared,
            stderr,
            process: Mutex::new(Some(process)),
            shutdown: Mutex::new(Some(shutdown_tx)),
        });
        Ok((connection, events_rx))
    }

    pub fn caps(&self) -> &AgentCaps {
        &self.caps
    }

    pub fn command(&self) -> &AgentCommand {
        &self.command
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr.snapshot()
    }

    pub async fn authenticate(&self, method_id: acp::AuthMethodId) -> Result<(), AcpError> {
        self.cx
            .send_request(acp::AuthenticateRequest::new(method_id))
            .block_task()
            .await?;
        Ok(())
    }

    pub async fn logout(&self) -> Result<(), AcpError> {
        if !self.caps.logout() {
            return Err(AcpError::unsupported("logout"));
        }
        self.cx
            .send_request(acp::LogoutRequest::new())
            .block_task()
            .await?;
        Ok(())
    }

    pub async fn new_session(
        &self,
        request: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, AcpError> {
        Ok(self.cx.send_request(request).block_task().await?)
    }

    /// Start `session/load`. Replayed updates arrive as events, followed by
    /// [`AgentEvent::LoadFinished`].
    pub fn load_session(&self, request: acp::LoadSessionRequest) -> Result<LoadId, AcpError> {
        if !self.caps.load_session() {
            return Err(AcpError::unsupported("session/load"));
        }
        let load_id = LoadId(self.shared.next_id());
        let session_id = request.session_id.clone();
        let shared = Arc::clone(&self.shared);
        self.cx
            .prepare_request(request)
            .on_receiving_result(async move |result| {
                shared.emit(AgentEvent::LoadFinished {
                    session_id,
                    load_id,
                    result: result.map_err(AcpError::from),
                });
                Ok(())
            })?;
        Ok(load_id)
    }

    pub async fn resume_session(
        &self,
        request: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, AcpError> {
        if !self.caps.resume_session() {
            return Err(AcpError::unsupported("session/resume"));
        }
        Ok(self.cx.send_request(request).block_task().await?)
    }

    pub async fn list_sessions(
        &self,
        request: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, AcpError> {
        if !self.caps.list_sessions() {
            return Err(AcpError::unsupported("session/list"));
        }
        Ok(self.cx.send_request(request).block_task().await?)
    }

    /// `session/close`. An unknown session counts as closed.
    pub async fn close_session(&self, session_id: acp::SessionId) -> Result<(), AcpError> {
        if !self.caps.close_session() {
            return Err(AcpError::unsupported("session/close"));
        }
        match self
            .cx
            .send_request(acp::CloseSessionRequest::new(session_id))
            .block_task()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                let err = AcpError::from(err);
                if err.is_resource_not_found() {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    /// `session/delete`. An unknown session counts as deleted.
    pub async fn delete_session(&self, session_id: acp::SessionId) -> Result<(), AcpError> {
        if !self.caps.delete_session() {
            return Err(AcpError::unsupported("session/delete"));
        }
        match self
            .cx
            .send_request(acp::DeleteSessionRequest::new(session_id))
            .block_task()
            .await
        {
            Ok(_) => Ok(()),
            Err(err) => {
                let err = AcpError::from(err);
                if err.is_resource_not_found() {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    pub async fn set_config_option(
        &self,
        request: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, AcpError> {
        Ok(self.cx.send_request(request).block_task().await?)
    }

    pub async fn set_mode(&self, request: acp::SetSessionModeRequest) -> Result<(), AcpError> {
        self.cx.send_request(request).block_task().await?;
        Ok(())
    }

    /// Start a prompt turn. Its completion arrives as
    /// [`AgentEvent::PromptFinished`] after every update the agent sent first.
    pub fn prompt(&self, request: acp::PromptRequest) -> Result<PromptId, AcpError> {
        let prompt_id = PromptId(self.shared.next_id());
        let session_id = request.session_id.clone();
        let shared = Arc::clone(&self.shared);
        self.cx
            .prepare_request(request)
            .on_receiving_result(async move |result| {
                shared.emit(AgentEvent::PromptFinished {
                    session_id,
                    prompt_id,
                    result: result.map_err(AcpError::from),
                });
                Ok(())
            })?;
        Ok(prompt_id)
    }

    /// `session/cancel`, then answer every request the agent left pending
    /// for this session as cancelled (prompt-turn.mdx, "Cancellation").
    pub fn cancel(&self, session_id: &acp::SessionId) -> Result<(), AcpError> {
        self.cx
            .send_notification(acp::CancelNotification::new(session_id.clone()))?;
        let abandoned: Vec<(PendingRequestId, Pending)> = self
            .shared
            .pending()
            .extract_if(|_, pending| pending.session_id() == Some(session_id))
            .collect();
        for (id, pending) in abandoned {
            pending.resolve_cancelled();
            self.shared
                .emit(AgentEvent::PendingRequestResolved { id });
        }
        Ok(())
    }

    /// Answer a pending permission request. Returns false if it is gone.
    pub fn respond_permission(
        &self,
        id: PendingRequestId,
        outcome: acp::RequestPermissionOutcome,
    ) -> bool {
        match self.shared.take(id) {
            Some(Pending::Permission { responder, .. }) => {
                if let Err(err) = responder.respond(acp::RequestPermissionResponse::new(outcome)) {
                    tracing::warn!("failed to answer permission request: {err:?}");
                }
                true
            }
            Some(other) => {
                self.shared.pending().insert(id, other);
                false
            }
            None => false,
        }
    }

    /// Answer a pending elicitation. Returns false if it is gone.
    pub fn respond_elicitation(
        &self,
        id: PendingRequestId,
        response: acp::CreateElicitationResponse,
    ) -> bool {
        match self.shared.take(id) {
            Some(Pending::Elicitation { responder, .. }) => {
                if let Err(err) = responder.respond(response) {
                    tracing::warn!("failed to answer elicitation: {err:?}");
                }
                true
            }
            Some(other) => {
                self.shared.pending().insert(id, other);
                false
            }
            None => false,
        }
    }

    /// Close stdin, give the agent a moment to exit, then end its group.
    pub async fn shutdown(&self) {
        if let Some(shutdown) = self
            .shutdown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = shutdown.send(());
        }
        let process = self
            .process
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut process) = process {
            process.wait(Duration::from_secs(1)).await;
            // The leader may be gone while its children (npx, node, ...) linger.
            process.terminate(Duration::from_secs(2)).await;
        }
    }
}

fn elicitation_session(request: &acp::CreateElicitationRequest) -> Option<acp::SessionId> {
    let scope = match &request.mode {
        acp::ElicitationMode::Form(form) => &form.scope,
        acp::ElicitationMode::Url(url) => &url.scope,
        acp::ElicitationMode::Other(other) => &other.scope,
        _ => return None,
    };
    match scope {
        acp::ElicitationScope::Session(scope) => Some(scope.session_id.clone()),
        _ => None,
    }
}
