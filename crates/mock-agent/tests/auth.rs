//! Authentication end to end: agent-type `authenticate`/`logout` and the
//! terminal login mode.

mod common;

use agent_client_protocol::ErrorCode;
use agent_client_protocol::v1::schema as acp;
use pretty_assertions::assert_eq;
use serde_json::json;

use common::MockAgent;

fn auth_scenario() -> serde_json::Value {
    json!({
        "require_auth": true,
        "auth_methods": [
            {"id": "login", "name": "Log in"},
            {"type": "terminal", "id": "tlogin", "name": "Terminal login", "args": ["--login"], "env": {}}
        ]
    })
}

#[tokio::test]
async fn agent_auth_gates_sessions_until_authenticated() {
    let agent = MockAgent::new(Some(auth_scenario()));
    let cwd = agent.cwd().to_path_buf();
    let marker = agent.state.path().join("authenticated");
    agent
        .connect(async |conn| {
            let init = conn.initialize(acp::ClientCapabilities::new()).await;
            let ids: Vec<String> = init
                .auth_methods
                .iter()
                .map(|m| m.id().to_string())
                .collect();
            assert_eq!(
                ids,
                vec!["login"],
                "terminal auth needs the client capability"
            );

            let err = conn
                .request(acp::NewSessionRequest::new(&cwd))
                .await
                .expect_err("auth required");
            assert_eq!(err.code, ErrorCode::AUTH_REQUIRED);
            let err = conn
                .request(acp::AuthenticateRequest::new("tlogin"))
                .await
                .expect_err("terminal method");
            assert_eq!(err.code, ErrorCode::INVALID_PARAMS);

            conn.request(acp::AuthenticateRequest::new("login")).await?;
            assert!(marker.exists());
            let id = conn
                .request(acp::NewSessionRequest::new(&cwd))
                .await?
                .session_id;
            conn.prompt(&id, "hello").await?;

            conn.request(acp::LogoutRequest::new()).await?;
            assert!(!marker.exists());
            let err = conn
                .request(acp::NewSessionRequest::new(&cwd))
                .await
                .expect_err("logged out");
            assert_eq!(err.code, ErrorCode::AUTH_REQUIRED);
            let err = conn.prompt(&id, "hello").await.expect_err("prompt");
            assert_eq!(err.code, ErrorCode::AUTH_REQUIRED);
            let err = conn
                .request(acp::LoadSessionRequest::new(id.clone(), &cwd))
                .await
                .expect_err("load");
            assert_eq!(err.code, ErrorCode::AUTH_REQUIRED);
            let err = conn
                .request(acp::ResumeSessionRequest::new(id.clone(), &cwd))
                .await
                .expect_err("resume");
            assert_eq!(err.code, ErrorCode::AUTH_REQUIRED);
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn terminal_login_authenticates_later_agents() {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    let agent = MockAgent::new(Some(auth_scenario()));
    let cwd = agent.cwd().to_path_buf();
    let mut login = agent.command().arg("--login").spawn().expect("spawn login");
    let mut stdin = login.stdin.take().expect("stdin");
    stdin.write_all(b"\n").await.expect("write");
    drop(stdin);
    let mut stdout = String::new();
    login
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .await
        .expect("read");
    let status = tokio::time::timeout(common::STEP_TIMEOUT, login.wait())
        .await
        .expect("login timed out")
        .expect("wait");
    assert!(status.success());
    assert!(
        stdout.contains("press Enter") && stdout.contains("Logged in."),
        "{stdout}"
    );
    assert!(agent.state.path().join("authenticated").exists());

    agent
        .connect(async |conn| {
            let caps =
                acp::ClientCapabilities::new().auth(acp::AuthCapabilities::new().terminal(true));
            let init = conn.initialize(caps).await;
            assert_eq!(init.auth_methods.len(), 2);
            conn.request(acp::NewSessionRequest::new(&cwd)).await?;
            Ok(())
        })
        .await;
}

#[tokio::test]
async fn terminal_login_needs_a_state_directory() {
    use tokio::io::AsyncReadExt;

    let agent = MockAgent::new(Some(auth_scenario()));
    let mut login = agent
        .command()
        .env_remove("ACPC_MOCK_STATE")
        .arg("--login")
        .spawn()
        .expect("spawn login");
    drop(login.stdin.take());
    let mut stdout = String::new();
    login
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .await
        .expect("read");
    let status = tokio::time::timeout(common::STEP_TIMEOUT, login.wait())
        .await
        .expect("login timed out")
        .expect("wait");
    assert_eq!(status.code(), Some(1));
    assert_eq!(stdout, "", "no prompt and no success message");
}
