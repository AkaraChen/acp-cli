//! Agent-wide state, initialization and authentication.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use agent_client_protocol::Client;
use agent_client_protocol::Error;
use agent_client_protocol::ErrorCode;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema as acp;
use tokio_util::sync::CancellationToken;

use crate::scenario::Scenario;
use crate::scenario::SessionRecord;
use crate::store::StateDir;

pub type Cx = ConnectionTo<Client>;

pub struct MockAgent {
    pub scenario: Scenario,
    state: StateDir,
    inner: Mutex<Inner>,
    /// Serializes snapshot-and-write (and delete) so the newest snapshot of
    /// a session always lands last and a deleted session is never rewritten.
    persist_lock: Mutex<()>,
    id_prefix: String,
    next_id: AtomicU64,
}

#[derive(Default)]
pub(crate) struct Inner {
    pub client: acp::ClientCapabilities,
    pub authenticated: bool,
    pub sessions: BTreeMap<String, Session>,
}

pub(crate) struct Session {
    pub record: SessionRecord,
    /// Written to `sessions.json` (created or modified by this agent).
    pub persist: bool,
    /// Set up on this connection via new/load/resume.
    pub active: bool,
    pub config_options: Vec<acp::SessionConfigOption>,
    pub modes: Option<acp::SessionModeState>,
    /// The running prompt turn, if any.
    pub turn: Option<TurnHandle>,
    pub used_tokens: u64,
}

/// Controls a running prompt turn.
#[derive(Clone)]
pub(crate) struct TurnHandle {
    /// Cancelled by `session/cancel`, `$/cancel_request`, close and delete.
    pub cancel: CancellationToken,
    /// Cancelled by the turn once it has answered `session/prompt`.
    pub done: CancellationToken,
}

impl TurnHandle {
    pub fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            done: CancellationToken::new(),
        }
    }

    /// Cancels the turn; the returned token fires once it has answered.
    pub fn cancel(&self) -> CancellationToken {
        self.cancel.cancel();
        self.done.clone()
    }
}

impl Session {
    pub fn new(record: SessionRecord, persist: bool, scenario: &Scenario) -> Self {
        Self {
            record,
            persist,
            active: false,
            config_options: scenario.config_options.clone(),
            modes: scenario.modes.clone(),
            turn: None,
            used_tokens: 0,
        }
    }
}

impl MockAgent {
    pub fn new(scenario: Scenario, state: StateDir) -> Self {
        let mut sessions = BTreeMap::new();
        for record in &scenario.sessions {
            let session = Session::new(record.clone(), false, &scenario);
            sessions.insert(record.session_id.clone(), session);
        }
        match state.load_sessions() {
            Ok(records) => {
                for record in records {
                    let session = Session::new(record, true, &scenario);
                    sessions.insert(session.record.session_id.clone(), session);
                }
            }
            Err(err) => log!("ignoring unreadable stored sessions: {err}"),
        }
        // The pid keeps ids apart for agents started in the same millisecond
        // that share a state directory.
        let millis = chrono::Utc::now().timestamp_millis();
        let pid = std::process::id();
        Self {
            scenario,
            state,
            inner: Mutex::new(Inner {
                sessions,
                ..Inner::default()
            }),
            persist_lock: Mutex::new(()),
            id_prefix: format!("{millis:x}_{pid:x}"),
            next_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A unique id such as `sess_18f3a2b4c5d_3039_1`.
    pub fn unique_id(&self, kind: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{kind}_{}_{n}", self.id_prefix)
    }

    pub(crate) fn require_auth(&self) -> Result<(), Error> {
        let authenticated = self.lock().authenticated || self.state.is_authenticated();
        if self.scenario.require_auth && !authenticated {
            return Err(Error::auth_required());
        }
        Ok(())
    }

    fn persist_guard(&self) -> MutexGuard<'_, ()> {
        self.persist_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes the session to the state directory if it is persistent. Each
    /// session has its own file, so agents sharing the directory only ever
    /// overwrite the sessions they changed themselves.
    pub(crate) fn persist(&self, session_id: &str) {
        let _guard = self.persist_guard();
        let record = self
            .lock()
            .sessions
            .get(session_id)
            .filter(|session| session.persist)
            .map(|session| session.record.clone());
        if let Some(record) = record
            && let Err(err) = self.state.save_session(&record)
        {
            log!("failed to save session {session_id}: {err}");
        }
    }

    /// Forgets the session and removes its file from the state directory.
    pub(crate) fn remove_session(&self, session_id: &str) -> Option<Session> {
        let _guard = self.persist_guard();
        let session = self.lock().sessions.remove(session_id)?;
        if let Err(err) = self.state.delete_session(session_id) {
            log!("failed to delete session {session_id}: {err}");
        }
        Some(session)
    }

    /// Appends an update the agent sent to the session's replayable history.
    pub(crate) fn record(&self, session_id: &acp::SessionId, update: acp::SessionUpdate) {
        if let Some(session) = self.lock().sessions.get_mut(&*session_id.0) {
            session.record.history.push(update);
        }
    }

    pub(crate) fn finish_turn(&self, session_id: &acp::SessionId) {
        if let Some(session) = self.lock().sessions.get_mut(&*session_id.0) {
            session.turn = None;
        }
        self.persist(&session_id.0);
    }

    // ---- initialization and auth ----

    pub fn initialize(&self, request: acp::InitializeRequest) -> acp::InitializeResponse {
        let terminal_auth = request.client_capabilities.auth.terminal;
        self.lock().client = request.client_capabilities;
        let caps = &self.scenario.capabilities;
        let mut sessions = acp::SessionCapabilities::new();
        if caps.list {
            sessions = sessions.list(acp::SessionListCapabilities::new());
        }
        if caps.resume {
            sessions = sessions.resume(acp::SessionResumeCapabilities::new());
        }
        if caps.close {
            sessions = sessions.close(acp::SessionCloseCapabilities::new());
        }
        if caps.delete {
            sessions = sessions.delete(acp::SessionDeleteCapabilities::new());
        }
        let mut agent = acp::AgentCapabilities::new()
            .load_session(caps.load_session)
            .prompt_capabilities(
                acp::PromptCapabilities::new()
                    .image(caps.image)
                    .audio(caps.audio)
                    .embedded_context(caps.embedded_context),
            )
            .mcp_capabilities(
                acp::McpCapabilities::new()
                    .http(caps.mcp_http)
                    .sse(caps.mcp_sse),
            )
            .session_capabilities(sessions);
        if caps.logout {
            agent = agent
                .auth(acp::AgentAuthCapabilities::new().logout(acp::LogoutCapabilities::new()));
        }
        let info = &self.scenario.agent_info;
        acp::InitializeResponse::new(ProtocolVersion::V1)
            .agent_capabilities(agent)
            .auth_methods(self.auth_methods(terminal_auth))
            .agent_info(
                acp::Implementation::new(info.name.as_str(), info.version.as_str())
                    .title(info.title.clone()),
            )
    }

    /// Terminal methods are only advertised to clients that support them.
    fn auth_methods(&self, terminal_auth: bool) -> Vec<acp::AuthMethod> {
        self.scenario
            .auth_methods
            .iter()
            .filter(|method| terminal_auth || !matches!(method, acp::AuthMethod::Terminal(_)))
            .cloned()
            .collect()
    }

    pub fn authenticate(
        &self,
        request: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, Error> {
        let terminal_auth = self.lock().client.auth.terminal;
        let methods = self.auth_methods(terminal_auth);
        match methods
            .iter()
            .find(|method| *method.id() == request.method_id)
        {
            None => Err(invalid_params(format!(
                "unknown auth method `{}`",
                request.method_id
            ))),
            Some(acp::AuthMethod::Terminal(_)) => Err(invalid_params(
                "terminal auth methods are run by the client, not passed to authenticate",
            )),
            Some(_) => {
                self.lock().authenticated = true;
                if let Err(err) = self.state.set_authenticated(true) {
                    log!("failed to write the auth marker: {err}");
                }
                Ok(acp::AuthenticateResponse::new())
            }
        }
    }

    pub fn logout(&self) -> Result<acp::LogoutResponse, Error> {
        if !self.scenario.capabilities.logout {
            return Err(Error::method_not_found());
        }
        self.lock().authenticated = false;
        if let Err(err) = self.state.set_authenticated(false) {
            log!("failed to remove the auth marker: {err}");
        }
        Ok(acp::LogoutResponse::new())
    }
}

/// Boolean options are only shown to clients advertising
/// `session.configOptions.boolean`. `None` when nothing is left.
pub(crate) fn visible_options(
    client: &acp::ClientCapabilities,
    options: &[acp::SessionConfigOption],
) -> Option<Vec<acp::SessionConfigOption>> {
    let booleans = client
        .session
        .as_ref()
        .and_then(|session| session.config_options.as_ref())
        .is_some_and(|config| config.boolean.is_some());
    let visible: Vec<_> = options
        .iter()
        .filter(|option| booleans || !matches!(option.kind, acp::SessionConfigKind::Boolean(_)))
        .cloned()
        .collect();
    (!visible.is_empty()).then_some(visible)
}

pub(crate) fn invalid_params(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::INVALID_PARAMS, message)
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
