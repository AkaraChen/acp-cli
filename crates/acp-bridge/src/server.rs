//! The bridge actor: transport plumbing, JSON-RPC bookkeeping, dispatch.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use acp_client::AgentEvent;
use acp_client::ClientOptions;
use acp_client::schema as acp;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerNotificationEnvelope;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestResolvedNotification;
use codex_app_server_transport::CHANNEL_CAPACITY;
use codex_app_server_transport::ConnectionId;
use codex_app_server_transport::DaemonShutdownAccess;
use codex_app_server_transport::OutgoingError;
use codex_app_server_transport::OutgoingMessage;
use codex_app_server_transport::OutgoingResponse;
use codex_app_server_transport::QueuedOutgoingMessage;
use codex_app_server_transport::TransportEvent;
use codex_app_server_transport::WebSocketCompression;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agents::AgentProfile;
use crate::agents::Agents;
use crate::agents::ConnectedAgent;
use crate::approvals::AgentRequest;
use crate::approvals::AgentRequestKey;
use crate::threads::Threads;

pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const INTERNAL_ERROR: i64 = -32603;

/// How long a stdio client's in-flight requests and turns may run after it
/// closes stdin (upstream's app-server drains RPCs for the same time).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Static configuration for one bridge instance.
#[derive(Clone)]
pub struct BridgeOptions {
    pub agents: Vec<AgentProfile>,
    pub default_agent: String,
    /// Reported to clients as `codexHome`.
    pub codex_home: AbsolutePathBuf,
    /// Used when a client does not send a working directory.
    pub default_cwd: AbsolutePathBuf,
    pub client: ClientOptions,
    pub mcp_servers: Vec<acp::McpServer>,
    pub user_agent: String,
    /// Filled with the ids of threads whose agent can list and reopen
    /// sessions, i.e. the ones `acpc resume -a <agent> <id>` can continue,
    /// mapped to that agent's slug.
    pub resumable: Arc<Mutex<HashMap<String, String>>>,
    /// Approve every agent permission request without asking (picking the
    /// agent's allow option), and report "never ask, full access" so the
    /// TUI shows YOLO mode. Elicitations are still shown.
    pub yolo: bool,
}

pub enum BridgeTransport {
    /// Serve the Codex TUI's remote endpoint (WebSocket over a unix socket).
    UnixSocket(AbsolutePathBuf),
    /// Serve one client over stdin/stdout, like `codex app-server`.
    Stdio,
    /// Transport events supplied by the caller (in-process clients, tests).
    Channel(mpsc::Receiver<TransportEvent>),
}

/// Bind the transport, then run the bridge in the background until
/// `shutdown` is cancelled (or the stdio client disconnects).
pub async fn start(
    options: BridgeOptions,
    transport: BridgeTransport,
    initial_agent: Option<ConnectedAgent>,
    shutdown: CancellationToken,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let (transport_tx, mut transport_rx) = mpsc::channel::<TransportEvent>(CHANNEL_CAPACITY);
    // One client that can hang up while work is in flight (stdin EOF).
    let single_client = !matches!(transport, BridgeTransport::UnixSocket(_));
    match transport {
        BridgeTransport::Channel(events) => {
            transport_rx = events;
            drop(transport_tx);
        }
        BridgeTransport::UnixSocket(path) => {
            codex_app_server_transport::start_control_socket_acceptor(
                path,
                transport_tx,
                shutdown.clone(),
                DaemonShutdownAccess::Disabled,
                WebSocketCompression::Plain,
            )
            .await?;
        }
        BridgeTransport::Stdio => {
            let (name_tx, _name_rx) = oneshot::channel();
            codex_app_server_transport::start_stdio_connection(
                transport_tx,
                name_tx,
                /*install_shutdown_signal_handler*/ false,
            )
            .await?;
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Input>();
    let mut bridge = Bridge::new(Arc::new(options), tx);
    if let Some(agent) = initial_agent {
        bridge.adopt_connected_agent(agent);
    }
    Ok(tokio::spawn(async move {
        let mut drain_until: Option<tokio::time::Instant> = None;
        loop {
            if drain_until.is_some() && bridge.drained() {
                break;
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep_until(drain_until.unwrap_or_else(tokio::time::Instant::now)),
                    if drain_until.is_some() => break,
                event = transport_rx.recv(), if drain_until.is_none() => match event {
                    Some(TransportEvent::ConnectionClosed { .. }) | None if single_client => {
                        // Finish what the client is still waiting for, then stop.
                        bridge.begin_drain();
                        drain_until = Some(tokio::time::Instant::now() + DRAIN_TIMEOUT);
                    }
                    None => break,
                    Some(event) => bridge.on_transport_event(event),
                },
                input = rx.recv() => {
                    let Some(input) = input else { break };
                    bridge.on_input(input);
                }
            }
        }
        bridge.close_connections().await;
        bridge.shutdown().await;
    }))
}

/// [`start`], then wait for the bridge to stop.
pub async fn serve(
    options: BridgeOptions,
    transport: BridgeTransport,
    initial_agent: Option<ConnectedAgent>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    start(options, transport, initial_agent, shutdown)
        .await?
        .await?;
    Ok(())
}

/// Work submitted back to the actor from spawned tasks.
pub(crate) type Apply = Box<dyn FnOnce(&mut Bridge) + Send>;

pub(crate) enum Input {
    Agent {
        slug: String,
        generation: u64,
        // Boxed: agent events are large, and every Apply would carry their size.
        event: Box<AgentEvent>,
    },
    Apply(Apply),
}

struct Connection {
    out: mpsc::UnboundedSender<OutgoingMessage>,
    /// Ends once everything sent to `out` is written.
    forwarder: tokio::task::JoinHandle<()>,
    initialized: bool,
}

type ServerRequestCallback =
    Box<dyn FnOnce(&mut Bridge, Result<serde_json::Value, JSONRPCErrorError>) + Send>;

struct PendingServerRequest {
    thread_id: Option<String>,
    request: ServerRequest,
    on_response: ServerRequestCallback,
}

pub(crate) struct Bridge {
    pub(crate) options: Arc<BridgeOptions>,
    tx: mpsc::UnboundedSender<Input>,
    connections: HashMap<ConnectionId, Connection>,
    server_requests: HashMap<RequestId, PendingServerRequest>,
    next_server_request_id: i64,
    /// Client requests not answered yet.
    in_flight: RefCell<HashSet<(ConnectionId, RequestId)>>,
    /// The stdio client hung up; no one can answer server requests.
    draining: bool,
    /// Agent permission/elicitation requests shown to the client.
    pub(crate) agent_requests: HashMap<AgentRequestKey, AgentRequest>,
    pub(crate) agents: Agents,
    pub(crate) threads: Threads,
}

impl Bridge {
    fn new(options: Arc<BridgeOptions>, tx: mpsc::UnboundedSender<Input>) -> Self {
        let agents = Agents::new(&options);
        Self {
            options,
            tx,
            connections: HashMap::new(),
            server_requests: HashMap::new(),
            next_server_request_id: 1,
            in_flight: RefCell::default(),
            draining: false,
            agent_requests: HashMap::new(),
            agents,
            threads: Threads::default(),
        }
    }

    pub(crate) fn sender(&self) -> mpsc::UnboundedSender<Input> {
        self.tx.clone()
    }

    /// Run `fut` off the actor and apply its result on the actor.
    pub(crate) fn spawn<F, A>(&self, fut: F)
    where
        F: Future<Output = A> + Send + 'static,
        A: FnOnce(&mut Bridge) + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let apply = fut.await;
            let _ = tx.send(Input::Apply(Box::new(apply)));
        });
    }

    fn on_input(&mut self, input: Input) {
        match input {
            Input::Agent {
                slug,
                generation,
                event,
            } => self.on_agent_event(&slug, generation, *event),
            Input::Apply(apply) => apply(self),
        }
    }

    fn on_transport_event(&mut self, event: TransportEvent) {
        match event {
            TransportEvent::ConnectionOpened {
                connection_id,
                writer,
                ..
            } => {
                let (out, mut out_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
                let forwarder = tokio::spawn(async move {
                    let mut last_write = None;
                    while let Some(message) = out_rx.recv().await {
                        let (written_tx, written_rx) = oneshot::channel();
                        let message = QueuedOutgoingMessage {
                            message,
                            write_complete_tx: Some(written_tx),
                        };
                        if writer.send(message).await.is_err() {
                            return;
                        }
                        last_write = Some(written_rx);
                    }
                    if let Some(written) = last_write {
                        let _ = written.await;
                    }
                });
                self.connections.insert(
                    connection_id,
                    Connection {
                        out,
                        forwarder,
                        initialized: false,
                    },
                );
            }
            TransportEvent::ConnectionClosed { connection_id } => {
                self.connections.remove(&connection_id);
                self.in_flight
                    .borrow_mut()
                    .retain(|(conn, _)| *conn != connection_id);
            }
            TransportEvent::IncomingMessage {
                connection_id,
                message,
            } => self.on_message(connection_id, message),
            TransportEvent::DaemonShutdown => {}
        }
    }

    fn on_message(&mut self, conn: ConnectionId, message: JSONRPCMessage) {
        match message {
            JSONRPCMessage::Request(request) => {
                let id = request.id.clone();
                let method = request.method.clone();
                match ClientRequest::try_from(request) {
                    Ok(request) => {
                        tracing::debug!(%method, "client request");
                        self.in_flight.borrow_mut().insert((conn, id));
                        crate::requests::dispatch(self, conn, request);
                    }
                    Err(err) => {
                        tracing::debug!(%method, "unparsed client request: {err}");
                        self.respond_error(
                            conn,
                            id,
                            METHOD_NOT_FOUND,
                            format!("acpc does not support `{method}`"),
                        );
                    }
                }
            }
            JSONRPCMessage::Notification(notification) => {
                if notification.method == "initialized"
                    && let Some(connection) = self.connections.get_mut(&conn)
                {
                    connection.initialized = true;
                    crate::ext::agents_updated(self);
                }
            }
            JSONRPCMessage::Response(response) => {
                self.on_server_request_result(response.id, Ok(response.result));
            }
            JSONRPCMessage::Error(error) => {
                self.on_server_request_result(error.id, Err(error.error));
            }
        }
    }

    fn send(&self, conn: ConnectionId, message: OutgoingMessage) {
        if let Some(connection) = self.connections.get(&conn) {
            let _ = connection.out.send(message);
        }
    }

    pub(crate) fn respond(
        &self,
        conn: ConnectionId,
        id: RequestId,
        payload: impl Into<ClientResponsePayload>,
    ) {
        self.in_flight.borrow_mut().remove(&(conn, id.clone()));
        self.send(
            conn,
            OutgoingMessage::Response(OutgoingResponse {
                id,
                result: Box::new(payload.into()),
            }),
        );
    }

    pub(crate) fn respond_error(
        &self,
        conn: ConnectionId,
        id: RequestId,
        code: i64,
        message: impl Into<String>,
    ) {
        self.respond_error_data(conn, id, code, message, None);
    }

    pub(crate) fn respond_error_data(
        &self,
        conn: ConnectionId,
        id: RequestId,
        code: i64,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) {
        self.in_flight.borrow_mut().remove(&(conn, id.clone()));
        self.send(
            conn,
            OutgoingMessage::Error(OutgoingError {
                id,
                error: JSONRPCErrorError {
                    code,
                    data,
                    message: message.into(),
                },
            }),
        );
    }

    /// Broadcast a notification to every initialized client.
    pub(crate) fn notify(&self, notification: ServerNotification) {
        let envelope = ServerNotificationEnvelope {
            notification,
            emitted_at_ms: Some(crate::records::now_ms()),
        };
        for connection in self.connections.values().filter(|c| c.initialized) {
            let _ = connection
                .out
                .send(OutgoingMessage::AppServerNotification(envelope.clone()));
        }
    }

    /// Send a server request to every initialized client; the first answer
    /// wins and the others are told the request was resolved.
    pub(crate) fn request(
        &mut self,
        thread_id: Option<String>,
        make: impl FnOnce(RequestId) -> ServerRequest,
        on_response: ServerRequestCallback,
    ) -> RequestId {
        let id = RequestId::Integer(self.next_server_request_id);
        self.next_server_request_id += 1;
        let request = make(id.clone());
        if self.draining {
            // Nobody can answer; resolve it once the caller has recorded it.
            let id = id.clone();
            let _ = self
                .tx
                .send(Input::Apply(Box::new(move |bridge: &mut Bridge| {
                    bridge.on_server_request_result(id, Err(client_gone()));
                })));
        } else {
            for connection in self.connections.values().filter(|c| c.initialized) {
                let _ = connection
                    .out
                    .send(OutgoingMessage::Request(request.clone()));
            }
        }
        self.server_requests.insert(
            id.clone(),
            PendingServerRequest {
                thread_id,
                request,
                on_response,
            },
        );
        id
    }

    /// Send `thread_id`'s unanswered server requests to one client again
    /// (it dropped them while it was showing another thread).
    pub(crate) fn resend_server_requests(&self, conn: ConnectionId, thread_id: &str) {
        let mut pending: Vec<&PendingServerRequest> = self
            .server_requests
            .values()
            .filter(|pending| pending.thread_id.as_deref() == Some(thread_id))
            .collect();
        pending.sort_by_key(|pending| match pending.request.id() {
            RequestId::Integer(id) => *id,
            RequestId::String(_) => i64::MAX,
        });
        for pending in pending {
            self.send(conn, OutgoingMessage::Request(pending.request.clone()));
        }
    }

    /// Withdraw a server request nobody needs to answer anymore.
    pub(crate) fn resolve_server_request(&mut self, id: &RequestId) {
        if let Some(pending) = self.server_requests.remove(id)
            && let Some(thread_id) = pending.thread_id
        {
            self.notify(ServerNotification::ServerRequestResolved(
                ServerRequestResolvedNotification {
                    thread_id,
                    request_id: id.clone(),
                },
            ));
        }
    }

    fn on_server_request_result(
        &mut self,
        id: RequestId,
        result: Result<serde_json::Value, JSONRPCErrorError>,
    ) {
        let Some(pending) = self.server_requests.remove(&id) else {
            return;
        };
        let _ = &pending.request;
        if let Some(thread_id) = pending.thread_id.clone() {
            self.notify(ServerNotification::ServerRequestResolved(
                ServerRequestResolvedNotification {
                    thread_id,
                    request_id: id,
                },
            ));
        }
        (pending.on_response)(self, result);
    }

    fn adopt_connected_agent(&mut self, agent: ConnectedAgent) {
        let tx = self.sender();
        self.agents.adopt(agent, tx);
    }

    fn on_agent_event(&mut self, slug: &str, generation: u64, event: AgentEvent) {
        if !self.agents.is_current(slug, generation) {
            return;
        }
        match event {
            AgentEvent::Exited { reason, stderr } => {
                self.agents.mark_exited(slug);
                crate::threads::on_agent_exited(self, slug, &reason, &stderr);
            }
            event => crate::threads::on_agent_event(self, slug, event),
        }
    }

    /// The stdio client closed stdin: it can no longer answer server
    /// requests, but still reads what is in flight.
    fn begin_drain(&mut self) {
        self.draining = true;
        let ids: Vec<RequestId> = self.server_requests.keys().cloned().collect();
        for id in ids {
            self.on_server_request_result(id, Err(client_gone()));
        }
    }

    fn drained(&self) -> bool {
        self.in_flight.borrow().is_empty() && !self.threads.any_active()
    }

    /// Stop accepting output and wait until what was sent is written.
    async fn close_connections(&mut self) {
        let forwarders: Vec<_> = self
            .connections
            .drain()
            .map(|(_, connection)| connection.forwarder)
            .collect();
        for forwarder in forwarders {
            let _ = tokio::time::timeout(Duration::from_secs(5), forwarder).await;
        }
    }

    async fn shutdown(&mut self) {
        self.agents.shutdown_all().await;
    }
}

fn client_gone() -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: INTERNAL_ERROR,
        data: None,
        message: "the client disconnected".to_string(),
    }
}
