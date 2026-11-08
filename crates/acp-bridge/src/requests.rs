//! Client request dispatch.
//!
//! Requests the TUI cannot do without get real answers. Requests that only
//! feed optional Codex features (skills, plugins, MCP status, ...) get empty
//! answers. Everything else is `-32601`, which the TUI treats as
//! "unsupported" for the flows acpc keeps reachable.

use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::CollaborationModeListResponse;
use codex_app_server_protocol::ConfigRequirementsReadResponse;
use codex_app_server_protocol::GetAccountResponse;
use codex_app_server_protocol::InitializeResponse;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadNameUpdatedNotification;
use codex_app_server_protocol::ThreadSetNameResponse;
use codex_app_server_protocol::ThreadSettingsUpdateResponse;
use codex_app_server_protocol::ThreadShellCommandResponse;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::ThreadUnsubscribeStatus;
use codex_app_server_protocol::TurnInterruptResponse;
use codex_app_server_protocol::WarningNotification;
use codex_app_server_transport::ConnectionId;
use serde::de::DeserializeOwned;

use crate::history;
use crate::models;
use crate::server::Bridge;
use crate::server::INTERNAL_ERROR;
use crate::server::INVALID_REQUEST;
use crate::server::METHOD_NOT_FOUND;
use crate::threads;
use crate::threads::TurnSettings;

/// Respond with a payload built from JSON; used for Codex-only responses
/// whose structs are large and irrelevant to acpc.
fn respond_json<T>(bridge: &Bridge, conn: ConnectionId, id: RequestId, value: serde_json::Value)
where
    T: DeserializeOwned + Into<ClientResponsePayload>,
{
    match serde_json::from_value::<T>(value) {
        Ok(response) => bridge.respond(conn, id, response),
        Err(err) => {
            tracing::warn!("failed to build stub response: {err}");
            bridge.respond_error(conn, id, METHOD_NOT_FOUND, "not supported by acpc");
        }
    }
}

pub(crate) fn dispatch(bridge: &mut Bridge, conn: ConnectionId, request: ClientRequest) {
    match request {
        ClientRequest::Initialize { request_id, .. } => {
            let response = InitializeResponse {
                user_agent: bridge.options.user_agent.clone(),
                codex_home: bridge.options.codex_home.clone(),
                platform_family: if cfg!(windows) { "windows" } else { "unix" }.to_string(),
                platform_os: std::env::consts::OS.to_string(),
            };
            bridge.respond(conn, request_id, response);
        }
        ClientRequest::GetAccount { request_id, .. } => {
            bridge.respond(
                conn,
                request_id,
                GetAccountResponse {
                    account: None,
                    user_id: None,
                    requires_openai_auth: false,
                    workspace_routing: None,
                },
            );
        }
        ClientRequest::LogoutAccount { request_id, .. } => logout(bridge, conn, request_id),
        ClientRequest::ConfigRequirementsRead { request_id, .. } => {
            bridge.respond(
                conn,
                request_id,
                ConfigRequirementsReadResponse {
                    supports_independent_speed_modes: None,
                    requirements: None,
                },
            );
        }
        ClientRequest::ConfigRead { request_id, .. } => {
            respond_json::<codex_app_server_protocol::ConfigReadResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "config": {}, "origins": {} }),
            );
        }
        // The TUI persists preferences (e.g. the default model) into its
        // config file. acpc's agent settings live in the agent session, so
        // accept and drop the write.
        ClientRequest::ConfigValueWrite { request_id, .. } => {
            let response = config_write_response(bridge);
            bridge.respond(conn, request_id, ClientResponsePayload::ConfigValueWrite(response));
        }
        ClientRequest::ConfigBatchWrite { request_id, .. } => {
            let response = config_write_response(bridge);
            bridge.respond(conn, request_id, ClientResponsePayload::ConfigBatchWrite(response));
        }
        ClientRequest::CollaborationModeList { request_id, .. } => {
            bridge.respond(
                conn,
                request_id,
                CollaborationModeListResponse { data: Vec::new() },
            );
        }
        ClientRequest::ModelList { request_id, .. } => model_list(bridge, conn, request_id),
        ClientRequest::SkillsList { request_id, .. } => {
            respond_json::<codex_app_server_protocol::SkillsListResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "data": [] }),
            );
        }
        // Codex-only catalogs the TUI refreshes in the background.
        ClientRequest::PluginInstalled { request_id, .. } => {
            respond_json::<codex_app_server_protocol::PluginInstalledResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "marketplaces": [], "marketplaceLoadErrors": [] }),
            );
        }
        ClientRequest::HooksList { request_id, .. } => {
            respond_json::<codex_app_server_protocol::HooksListResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "data": [] }),
            );
        }
        ClientRequest::AppsList { request_id, .. } => {
            respond_json::<codex_app_server_protocol::AppsListResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "data": [], "nextCursor": null }),
            );
        }
        ClientRequest::AppsInstalled { request_id, .. } => {
            respond_json::<codex_app_server_protocol::AppsInstalledResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "apps": [] }),
            );
        }
        ClientRequest::ExperimentalFeatureList { request_id, .. } => {
            respond_json::<codex_app_server_protocol::ExperimentalFeatureListResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "data": [], "nextCursor": null }),
            );
        }
        ClientRequest::PermissionProfileList { request_id, .. } => {
            respond_json::<codex_app_server_protocol::PermissionProfileListResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "data": [], "nextCursor": null }),
            );
        }
        ClientRequest::McpServerStatusList { request_id, .. } => {
            respond_json::<codex_app_server_protocol::ListMcpServerStatusResponse>(
                bridge,
                conn,
                request_id,
                serde_json::json!({ "data": [], "nextCursor": null }),
            );
        }
        ClientRequest::ThreadStart { request_id, params } => {
            if params.ephemeral == Some(true) {
                // Codex starts hidden side threads (e.g. title generation);
                // they would become real agent sessions.
                bridge.respond_error(conn, request_id, INVALID_REQUEST, "ephemeral threads are not supported");
                return;
            }
            let agent = params
                .config
                .as_ref()
                .and_then(|config| config.get(crate::ext::AGENT_CONFIG_KEY))
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| bridge.agents.current.clone(), str::to_string);
            let cwd = history::absolute_cwd(bridge, params.cwd.as_deref());
            threads::start(bridge, conn, request_id, agent, cwd);
        }
        ClientRequest::ThreadResume { request_id, params } => {
            history::resume(bridge, conn, request_id, params.thread_id);
        }
        ClientRequest::ThreadRead { request_id, params } => {
            history::read(bridge, conn, request_id, &params.thread_id, params.include_turns);
        }
        ClientRequest::ThreadList { request_id, params } => {
            history::list(bridge, conn, request_id, params);
        }
        ClientRequest::ThreadLoadedList { request_id, .. } => {
            bridge.respond(
                conn,
                request_id,
                ThreadLoadedListResponse {
                    data: bridge.threads.loaded_ids(),
                    next_cursor: None,
                },
            );
        }
        ClientRequest::ThreadUnsubscribe { request_id, params } => {
            let status = match bridge.threads.get(&params.thread_id) {
                Some(thread) if thread.loaded => ThreadUnsubscribeStatus::Unsubscribed,
                Some(_) => ThreadUnsubscribeStatus::NotSubscribed,
                None => ThreadUnsubscribeStatus::NotLoaded,
            };
            let unsubscribed = status == ThreadUnsubscribeStatus::Unsubscribed;
            bridge.respond(conn, request_id, ThreadUnsubscribeResponse { status });
            if unsubscribed {
                close_session(bridge, &params.thread_id);
            }
        }
        ClientRequest::ThreadDelete { request_id, params } => {
            delete_thread(bridge, conn, request_id, params.thread_id);
        }
        ClientRequest::ThreadSetName { request_id, params } => {
            if let Some(thread) = bridge.threads.get_mut(&params.thread_id) {
                thread.title = Some(params.name.clone());
            }
            bridge.respond(conn, request_id, ThreadSetNameResponse {});
            bridge.notify(ServerNotification::ThreadNameUpdated(
                ThreadNameUpdatedNotification {
                    thread_id: params.thread_id,
                    thread_name: Some(params.name),
                },
            ));
        }
        ClientRequest::ThreadSettingsUpdate { request_id, params } => {
            let settings = TurnSettings {
                model: params.model,
                effort: params.effort,
            };
            settings_update(bridge, conn, request_id, params.thread_id, settings);
        }
        ClientRequest::TurnStart { request_id, params } => {
            let settings = TurnSettings {
                model: params.model,
                effort: params.effort,
            };
            threads::start_turn(bridge, conn, request_id, &params.thread_id, params.input, settings);
        }
        ClientRequest::TurnInterrupt { request_id, params } => {
            match threads::interrupt(bridge, &params.thread_id) {
                Ok(()) => bridge.respond(conn, request_id, TurnInterruptResponse {}),
                Err(err) => bridge.respond_error(conn, request_id, INTERNAL_ERROR, err),
            }
        }
        ClientRequest::TurnSteer { request_id, .. } => {
            // ACP has no steering: make the TUI queue the input for the next
            // turn instead (it checks for this exact error shape).
            bridge.respond_error_data(
                conn,
                request_id,
                INVALID_REQUEST,
                "the agent cannot be steered mid-turn",
                Some(serde_json::json!({
                    "message": "the agent cannot be steered mid-turn",
                    "codexErrorInfo": "activeTurnNotSteerable",
                })),
            );
        }
        ClientRequest::ThreadShellCommand { request_id, params } => {
            // acpc does not run commands itself; agents own execution.
            bridge.respond(conn, request_id, ThreadShellCommandResponse {});
            bridge.notify(ServerNotification::Warning(WarningNotification {
                thread_id: Some(params.thread_id),
                message: "acpc does not run shell commands; ask the agent instead.".to_string(),
            }));
        }
        ClientRequest::AcpcAgentList { request_id, .. } => {
            let response = codex_app_server_protocol::AcpcAgentListResponse {
                agents: crate::ext::agents(bridge),
                current: bridge.agents.current.clone(),
            };
            bridge.respond(conn, request_id, response);
        }
        ClientRequest::AcpcAgentSelect { request_id, params } => {
            crate::select::select(bridge, conn, request_id, params.slug);
        }
        ClientRequest::AcpcSessionConfigSet { request_id, params } => {
            let value = match params.value {
                codex_app_server_protocol::AcpcSessionConfigInput::Boolean(value) => {
                    acp_client::schema::SessionConfigOptionValue::boolean(value)
                }
                codex_app_server_protocol::AcpcSessionConfigInput::Value(value) => {
                    acp_client::schema::SessionConfigOptionValue::value_id(value)
                }
            };
            set_config_option(bridge, conn, request_id, params.thread_id, params.config_id, value);
        }
        ClientRequest::AcpcModeSet { request_id, params } => {
            set_mode(bridge, conn, request_id, params.thread_id, params.mode_id);
        }
        other => {
            let method = other.method_name();
            tracing::debug!(%method, "unsupported client request");
            bridge.respond_error(
                conn,
                other.id().clone(),
                METHOD_NOT_FOUND,
                format!("acpc does not support `{method}`"),
            );
        }
    }
}

fn config_write_response(bridge: &Bridge) -> codex_app_server_protocol::ConfigWriteResponse {
    codex_app_server_protocol::ConfigWriteResponse {
        status: codex_app_server_protocol::WriteStatus::Ok,
        version: "0".to_string(),
        file_path: bridge.options.codex_home.join("config.toml"),
        overridden_metadata: None,
    }
}

fn model_list(bridge: &mut Bridge, conn: ConnectionId, id: RequestId) {
    // The catalog of the most recently active thread on the current agent.
    let current = bridge.agents.current.clone();
    let name = bridge
        .agents
        .profile(&current)
        .map_or_else(|| current.clone(), |profile| profile.name.clone());
    let thread = bridge
        .threads
        .on_agent(&current)
        .into_iter()
        .filter_map(|thread_id| bridge.threads.get(&thread_id))
        .filter(|thread| thread.loaded)
        .max_by_key(|thread| thread.updated_at);
    let data = match thread {
        Some(thread) => models::catalog(&thread.config_options, &thread.agent_name, thread.image_input),
        None => models::catalog(&[], &name, false),
    };
    bridge.respond(
        conn,
        id,
        ModelListResponse {
            data,
            next_cursor: None,
        },
    );
}

fn settings_update(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    thread_id: String,
    settings: TurnSettings,
) {
    let Some(thread) = bridge.threads.get(&thread_id) else {
        bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {thread_id}"));
        return;
    };
    let changes = settings.config_changes(&thread.config_options);
    let (Some(session_id), Some(connection)) = (
        thread.session_id.clone(),
        threads::connection_for(bridge, thread),
    ) else {
        bridge.respond(conn, id, ThreadSettingsUpdateResponse {});
        return;
    };
    bridge.spawn(async move {
        let mut options = None;
        let mut error = None;
        for (config_id, value) in changes {
            let request = acp_client::schema::SetSessionConfigOptionRequest::new(
                session_id.clone(),
                config_id,
                acp_client::schema::SessionConfigOptionValue::value_id(value),
            );
            match connection.set_config_option(request).await {
                Ok(response) => options = Some(response.config_options),
                Err(err) => error = Some(err.message),
            }
        }
        Box::new(move |bridge: &mut Bridge| {
            if let Some(options) = options
                && let Some(thread) = bridge.threads.get_mut(&thread_id)
            {
                thread.config_options = options;
                let notification = threads::settings_updated(thread);
                bridge.notify(notification);
            }
            match error {
                Some(message) => bridge.respond_error(conn, id, INTERNAL_ERROR, message),
                None => bridge.respond(conn, id, ThreadSettingsUpdateResponse {}),
            }
        })
    });
}

fn set_config_option(
    bridge: &mut Bridge,
    conn: ConnectionId,
    id: RequestId,
    thread_id: String,
    config_id: String,
    value: acp_client::schema::SessionConfigOptionValue,
) {
    let Some(thread) = bridge.threads.get(&thread_id) else {
        bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {thread_id}"));
        return;
    };
    let (Some(session_id), Some(connection)) = (
        thread.session_id.clone(),
        threads::connection_for(bridge, thread),
    ) else {
        bridge.respond_error(conn, id, INVALID_REQUEST, "thread is not loaded");
        return;
    };
    let request =
        acp_client::schema::SetSessionConfigOptionRequest::new(session_id, config_id, value);
    bridge.spawn(async move {
        let result = connection.set_config_option(request).await;
        move |bridge: &mut Bridge| match result {
            Ok(response) => {
                if let Some(thread) = bridge.threads.get_mut(&thread_id) {
                    thread.config_options = response.config_options;
                }
                bridge.respond(
                    conn,
                    id,
                    codex_app_server_protocol::AcpcSessionConfigSetResponse {},
                );
                if let Some(thread) = bridge.threads.get(&thread_id) {
                    bridge.notify(threads::settings_updated(thread));
                    let mut emit = Vec::new();
                    crate::ext::session_config_updated(thread, &mut emit);
                    threads::emit_all(bridge, emit);
                }
            }
            Err(err) => bridge.respond_error(conn, id, INTERNAL_ERROR, err.message),
        }
    });
}

fn set_mode(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, thread_id: String, mode_id: String) {
    let Some(thread) = bridge.threads.get(&thread_id) else {
        bridge.respond_error(conn, id, INVALID_REQUEST, format!("unknown thread {thread_id}"));
        return;
    };
    let (Some(session_id), Some(connection)) = (
        thread.session_id.clone(),
        threads::connection_for(bridge, thread),
    ) else {
        bridge.respond_error(conn, id, INVALID_REQUEST, "thread is not loaded");
        return;
    };
    let request = acp_client::schema::SetSessionModeRequest::new(session_id, mode_id.clone());
    bridge.spawn(async move {
        let result = connection.set_mode(request).await;
        move |bridge: &mut Bridge| match result {
            Ok(()) => {
                if let Some(modes) = bridge
                    .threads
                    .get_mut(&thread_id)
                    .and_then(|thread| thread.modes.as_mut())
                {
                    modes.current_mode_id = mode_id.into();
                }
                bridge.respond(conn, id, codex_app_server_protocol::AcpcModeSetResponse {});
                if let Some(thread) = bridge.threads.get(&thread_id) {
                    let mut emit = Vec::new();
                    crate::ext::session_config_updated(thread, &mut emit);
                    threads::emit_all(bridge, emit);
                }
            }
            Err(err) => bridge.respond_error(conn, id, INTERNAL_ERROR, err.message),
        }
    });
}

/// The client left an idle thread: release the agent session
/// (`session/close`) when the agent supports it. Resuming reloads it.
fn close_session(bridge: &mut Bridge, thread_id: &str) {
    let Some(thread) = bridge.threads.get(thread_id) else {
        return;
    };
    if thread.active.is_some() {
        return;
    }
    let (Some(session_id), Some(connection)) = (
        thread.session_id.clone(),
        threads::connection_for(bridge, thread),
    ) else {
        return;
    };
    if !connection.caps().close_session() {
        return;
    }
    if let Some(thread) = bridge.threads.get_mut(thread_id) {
        thread.loaded = false;
    }
    bridge.spawn(async move {
        if let Err(err) = connection.close_session(session_id).await {
            tracing::warn!("session/close failed: {err}");
        }
        |_: &mut Bridge| {}
    });
}

fn delete_thread(bridge: &mut Bridge, conn: ConnectionId, id: RequestId, thread_id: String) {
    let Some(thread) = bridge.threads.get(&thread_id) else {
        bridge.respond(conn, id, ThreadDeleteResponse {});
        return;
    };
    let (Some(session_id), Some(connection)) = (
        thread.session_id.clone(),
        threads::connection_for(bridge, thread),
    ) else {
        bridge.threads.remove(&thread_id);
        bridge.respond(conn, id, ThreadDeleteResponse {});
        return;
    };
    bridge.spawn(async move {
        let result = connection.delete_session(session_id).await;
        Box::new(move |bridge: &mut Bridge| match result {
            Ok(()) => {
                bridge.threads.remove(&thread_id);
                bridge.respond(conn, id, ThreadDeleteResponse {});
            }
            Err(err) => bridge.respond_error(conn, id, INTERNAL_ERROR, err.message),
        })
    });
}

fn logout(bridge: &mut Bridge, conn: ConnectionId, id: RequestId) {
    let Some(connection) = bridge.agents.connection(&bridge.agents.current.clone()) else {
        bridge.respond_error(conn, id, INTERNAL_ERROR, "the agent is not running");
        return;
    };
    bridge.spawn(async move {
        let result = connection.logout().await;
        Box::new(move |bridge: &mut Bridge| match result {
            Ok(()) => bridge.respond(conn, id, codex_app_server_protocol::LogoutAccountResponse {}),
            Err(err) => bridge.respond_error(conn, id, INTERNAL_ERROR, err.message),
        })
    });
}
