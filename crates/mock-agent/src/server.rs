//! Wires the ACP v1 agent role to [`MockAgent`].
//!
//! Every handler returns without awaiting the client, so the dispatch loop
//! stays free; prompt turns run on spawned tasks (see [`crate::prompt`]), and
//! so do close/delete responses that wait for a cancelled turn.

use std::sync::Arc;

use agent_client_protocol::Agent;
use agent_client_protocol::ConnectTo;
use agent_client_protocol::Error;
use agent_client_protocol::on_receive_notification;
use agent_client_protocol::on_receive_request;
use agent_client_protocol::v1::schema as acp;

use crate::agent::MockAgent;
use crate::prompt;

/// Serves ACP on `transport` until the client disconnects.
pub async fn serve(
    agent: Arc<MockAgent>,
    transport: impl ConnectTo<Agent> + 'static,
) -> Result<(), Error> {
    // Each handler gets its own handle to the agent.
    let a = || Arc::clone(&agent);
    Agent
        .v1()
        .name("acpc-mock-agent")
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::InitializeRequest, responder, _cx| {
                    responder.respond(agent.initialize(request))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::AuthenticateRequest, responder, _cx| {
                    responder.respond_with_result(agent.authenticate(request))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |_request: acp::LogoutRequest, responder, _cx| {
                    responder.respond_with_result(agent.logout())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::NewSessionRequest, responder, cx| {
                    agent.new_session(request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::LoadSessionRequest, responder, cx| {
                    agent.load_session(request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::ResumeSessionRequest, responder, cx| {
                    agent.resume_session(request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::ListSessionsRequest, responder, _cx| {
                    responder.respond_with_result(agent.list_sessions(&request))
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::CloseSessionRequest, responder, cx| {
                    agent.close_session(&request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::DeleteSessionRequest, responder, cx| {
                    agent.delete_session(&request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::SetSessionModeRequest, responder, cx| {
                    agent.set_mode(request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::SetSessionConfigOptionRequest, responder, cx| {
                    agent.set_config_option(request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            {
                let agent = a();
                async move |request: acp::PromptRequest, responder, cx| {
                    prompt::prompt(&agent, request, responder, &cx)
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            {
                let agent = a();
                async move |notification: acp::CancelNotification, _cx| {
                    agent.cancel(&notification);
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await
}
