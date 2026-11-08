//! Session setup (`new`/`load`/`resume`) and management (`list`, `close`,
//! `delete`, modes, config options, cancel).

use std::path::Path;
use std::path::PathBuf;

use agent_client_protocol::Error;
use agent_client_protocol::JsonRpcResponse;
use agent_client_protocol::v1::Responder;
use agent_client_protocol::v1::schema as acp;
use tokio_util::sync::CancellationToken;

use crate::agent::Cx;
use crate::agent::Inner;
use crate::agent::MockAgent;
use crate::agent::Session;
use crate::agent::TurnHandle;
use crate::agent::invalid_params;
use crate::agent::now;
use crate::agent::visible_options;
use crate::scenario::SessionRecord;

/// The `modes` and `configOptions` of a setup response.
type SetupState = (
    Option<acp::SessionModeState>,
    Option<Vec<acp::SessionConfigOption>>,
);

impl MockAgent {
    fn check_setup(&self, cwd: &Path, mcp_servers: &[acp::McpServer]) -> Result<(), Error> {
        self.require_auth()?;
        if !cwd.is_absolute() {
            return Err(invalid_params(format!(
                "cwd must be an absolute path, got {}",
                cwd.display()
            )));
        }
        let caps = &self.scenario.capabilities;
        for server in mcp_servers {
            let unsupported = match server {
                acp::McpServer::Http(_) => !caps.mcp_http,
                acp::McpServer::Sse(_) => !caps.mcp_sse,
                _ => false,
            };
            if unsupported {
                return Err(invalid_params(
                    "MCP server transport not advertised in mcpCapabilities",
                ));
            }
        }
        Ok(())
    }

    pub fn new_session(
        &self,
        request: acp::NewSessionRequest,
        responder: Responder<acp::NewSessionResponse>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let result = self
            .check_setup(&request.cwd, &request.mcp_servers)
            .map(|()| self.create_session(request.cwd));
        self.finish_setup(responder, result, cx)
    }

    fn create_session(&self, cwd: PathBuf) -> (acp::NewSessionResponse, acp::SessionId) {
        let id = self.unique_id("sess");
        let record = SessionRecord {
            session_id: id.clone(),
            cwd,
            title: None,
            updated_at: Some(now()),
            history: Vec::new(),
        };
        let mut session = Session::new(record, true, &self.scenario);
        session.active = true;
        let response = {
            let mut inner = self.lock();
            let response = acp::NewSessionResponse::new(id.clone())
                .modes(session.modes.clone())
                .config_options(visible_options(&inner.client, &session.config_options));
            inner.sessions.insert(id.clone(), session);
            response
        };
        self.persist(&id);
        (response, acp::SessionId::new(id))
    }

    pub fn load_session(
        &self,
        request: acp::LoadSessionRequest,
        responder: Responder<acp::LoadSessionResponse>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let result = if self.scenario.capabilities.load_session {
            self.check_setup(&request.cwd, &request.mcp_servers)
                .and_then(|()| self.activate(&request.session_id, Some(cx)))
                .map(|(modes, options)| {
                    let response = acp::LoadSessionResponse::new()
                        .modes(modes)
                        .config_options(options);
                    (response, request.session_id)
                })
        } else {
            Err(Error::method_not_found())
        };
        self.finish_setup(responder, result, cx)
    }

    pub fn resume_session(
        &self,
        request: acp::ResumeSessionRequest,
        responder: Responder<acp::ResumeSessionResponse>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let result = if self.scenario.capabilities.resume {
            self.check_setup(&request.cwd, &request.mcp_servers)
                .and_then(|()| self.activate(&request.session_id, None))
                .map(|(modes, options)| {
                    let response = acp::ResumeSessionResponse::new()
                        .modes(modes)
                        .config_options(options);
                    (response, request.session_id)
                })
        } else {
            Err(Error::method_not_found())
        };
        self.finish_setup(responder, result, cx)
    }

    /// Marks a stored session active, replaying its history to `replay_to`
    /// before the caller responds. Notices are live events, not history, so
    /// any in a seeded history are skipped.
    fn activate(
        &self,
        session_id: &acp::SessionId,
        replay_to: Option<&Cx>,
    ) -> Result<SetupState, Error> {
        let mut inner = self.lock();
        let Inner {
            client, sessions, ..
        } = &mut *inner;
        let session = sessions
            .get_mut(&*session_id.0)
            .ok_or_else(|| acp::Error::session_not_found(session_id.clone()))?;
        session.active = true;
        if let Some(cx) = replay_to {
            for update in &session.record.history {
                if matches!(update, acp::SessionUpdate::Notice(_)) {
                    continue;
                }
                cx.send_notification(acp::SessionNotification::new(
                    session_id.clone(),
                    update.clone(),
                ))?;
            }
        }
        Ok((
            session.modes.clone(),
            visible_options(client, &session.config_options),
        ))
    }

    /// Responds, then announces the slash commands for the session.
    fn finish_setup<T: JsonRpcResponse>(
        &self,
        responder: Responder<T>,
        result: Result<(T, acp::SessionId), Error>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let (response, session_id) = match result {
            Ok(parts) => parts,
            Err(err) => return responder.respond_with_error(err),
        };
        responder.respond(response)?;
        if self.scenario.commands.is_empty() {
            return Ok(());
        }
        let update = acp::SessionUpdate::AvailableCommandsUpdate(
            acp::AvailableCommandsUpdate::new(self.scenario.commands.clone()),
        );
        cx.send_notification(acp::SessionNotification::new(session_id, update))
    }

    pub fn list_sessions(
        &self,
        request: &acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, Error> {
        if !self.scenario.capabilities.list {
            return Err(Error::method_not_found());
        }
        let inner = self.lock();
        let mut records: Vec<&SessionRecord> = inner
            .sessions
            .values()
            .map(|session| &session.record)
            .filter(|record| request.cwd.as_ref().is_none_or(|cwd| record.cwd == *cwd))
            .collect();
        records.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let sessions = records
            .into_iter()
            .map(|record| {
                acp::SessionInfo::new(record.session_id.clone(), record.cwd.clone())
                    .title(record.title.clone())
                    .updated_at(record.updated_at.clone())
            })
            .collect();
        Ok(acp::ListSessionsResponse::new(sessions))
    }

    /// Cancels the session's turn like `session/cancel` and answers once the
    /// turn has answered `session/prompt`. Only sessions active on this
    /// connection can be closed.
    pub fn close_session(
        &self,
        request: &acp::CloseSessionRequest,
        responder: Responder<acp::CloseSessionResponse>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let result = if self.scenario.capabilities.close {
            self.with_active(&request.session_id, |_, session| {
                session.active = false;
                Ok(session.turn.as_ref().map(TurnHandle::cancel))
            })
        } else {
            Err(Error::method_not_found())
        };
        respond_after_turn(responder, result, acp::CloseSessionResponse::new(), cx)
    }

    /// Forgets a stored session (active or not), cancelling its turn, and
    /// answers once the turn has answered `session/prompt`.
    pub fn delete_session(
        &self,
        request: &acp::DeleteSessionRequest,
        responder: Responder<acp::DeleteSessionResponse>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let result = if self.scenario.capabilities.delete {
            self.remove_session(&request.session_id.0)
                .map(|session| session.turn.as_ref().map(TurnHandle::cancel))
                .ok_or_else(|| acp::Error::session_not_found(request.session_id.clone()).into())
        } else {
            Err(Error::method_not_found())
        };
        respond_after_turn(responder, result, acp::DeleteSessionResponse::new(), cx)
    }

    /// Responds, then also reports the change as `current_mode_update`.
    pub fn set_mode(
        &self,
        request: acp::SetSessionModeRequest,
        responder: Responder<acp::SetSessionModeResponse>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let result = self.with_active(&request.session_id, |_, session| {
            let Some(modes) = session.modes.as_mut() else {
                return Err(Error::method_not_found());
            };
            if !modes
                .available_modes
                .iter()
                .any(|mode| mode.id == request.mode_id)
            {
                return Err(invalid_params(format!(
                    "unknown mode `{}`",
                    request.mode_id
                )));
            }
            modes.current_mode_id = request.mode_id.clone();
            Ok(())
        });
        if let Err(err) = result {
            return responder.respond_with_error(err);
        }
        responder.respond(acp::SetSessionModeResponse::new())?;
        let update =
            acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(request.mode_id));
        cx.send_notification(acp::SessionNotification::new(request.session_id, update))
    }

    /// Responds with the full option list, then also sends it as
    /// `config_option_update`.
    pub fn set_config_option(
        &self,
        request: acp::SetSessionConfigOptionRequest,
        responder: Responder<acp::SetSessionConfigOptionResponse>,
        cx: &Cx,
    ) -> Result<(), Error> {
        let result = self.with_active(&request.session_id, |client, session| {
            let option = session
                .config_options
                .iter_mut()
                .find(|option| option.id == request.config_id)
                .ok_or_else(|| {
                    invalid_params(format!("unknown config option `{}`", request.config_id))
                })?;
            apply_config_value(&mut option.kind, &request.value)?;
            Ok(visible_options(client, &session.config_options).unwrap_or_default())
        });
        let options = match result {
            Ok(options) => options,
            Err(err) => return responder.respond_with_error(err),
        };
        responder.respond(acp::SetSessionConfigOptionResponse::new(options.clone()))?;
        let update = acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(options));
        cx.send_notification(acp::SessionNotification::new(request.session_id, update))
    }

    pub fn cancel(&self, notification: &acp::CancelNotification) {
        let inner = self.lock();
        match inner
            .sessions
            .get(&*notification.session_id.0)
            .and_then(|session| session.turn.as_ref())
        {
            Some(turn) => turn.cancel.cancel(),
            None => log!(
                "session/cancel for {}: no turn running",
                notification.session_id
            ),
        }
    }

    /// Runs `f` on an active session, or fails with `session_not_found`.
    pub(crate) fn with_active<R>(
        &self,
        session_id: &acp::SessionId,
        f: impl FnOnce(&acp::ClientCapabilities, &mut Session) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let mut inner = self.lock();
        let Inner {
            client, sessions, ..
        } = &mut *inner;
        match sessions.get_mut(&*session_id.0) {
            Some(session) if session.active => f(client, session),
            _ => Err(acp::Error::session_not_found(session_id.clone()).into()),
        }
    }
}

/// Responds with `response` once `turn_done` (from [`TurnHandle::cancel`])
/// fires, or right away when no turn was running.
fn respond_after_turn<T: JsonRpcResponse + Send + 'static>(
    responder: Responder<T>,
    turn_done: Result<Option<CancellationToken>, Error>,
    response: T,
    cx: &Cx,
) -> Result<(), Error> {
    match turn_done {
        Err(err) => responder.respond_with_error(err),
        Ok(None) => responder.respond(response),
        Ok(Some(done)) => cx.spawn(async move {
            done.cancelled().await;
            responder.respond(response)
        }),
    }
}

fn apply_config_value(
    kind: &mut acp::SessionConfigKind,
    value: &acp::SessionConfigOptionValue,
) -> Result<(), Error> {
    match (kind, value) {
        (
            acp::SessionConfigKind::Select(select),
            acp::SessionConfigOptionValue::ValueId { value },
        ) => {
            if !select_contains(&select.options, value) {
                return Err(invalid_params(format!("unknown value `{value}`")));
            }
            select.current_value = value.clone();
            Ok(())
        }
        (
            acp::SessionConfigKind::Boolean(boolean),
            acp::SessionConfigOptionValue::Boolean { value },
        ) => {
            boolean.current_value = *value;
            Ok(())
        }
        _ => Err(invalid_params(
            "value type does not match the config option",
        )),
    }
}

fn select_contains(
    options: &acp::SessionConfigSelectOptions,
    value: &acp::SessionConfigValueId,
) -> bool {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => {
            options.iter().any(|option| option.value == *value)
        }
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| &group.options)
            .any(|option| option.value == *value),
        _ => false,
    }
}
