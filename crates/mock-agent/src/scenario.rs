//! The scenario file. Every field is optional; omitted fields fall back to
//! the built-in demo agent (see [`Scenario::default`]).

use std::path::Path;
use std::path::PathBuf;

use agent_client_protocol::v1::schema::AuthMethod;
use agent_client_protocol::v1::schema::AvailableCommand;
use agent_client_protocol::v1::schema::AvailableCommandInput;
use agent_client_protocol::v1::schema::SessionConfigOption;
use agent_client_protocol::v1::schema::SessionConfigOptionCategory;
use agent_client_protocol::v1::schema::SessionConfigSelectOption;
use agent_client_protocol::v1::schema::SessionMode;
use agent_client_protocol::v1::schema::SessionModeState;
use agent_client_protocol::v1::schema::SessionUpdate;
use agent_client_protocol::v1::schema::StopReason;
use agent_client_protocol::v1::schema::UnstructuredCommandInput;
use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

use crate::step::Step;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scenario {
    pub agent_info: AgentInfo,
    pub capabilities: Capabilities,
    pub auth_methods: Vec<AuthMethod>,
    /// Session setup and prompts fail with `auth_required` until authenticated.
    pub require_auth: bool,
    /// Persistent state directory; falls back to `$ACPC_MOCK_STATE`, then to
    /// in-memory state only.
    pub state_dir: Option<PathBuf>,
    pub config_options: Vec<SessionConfigOption>,
    pub modes: Option<SessionModeState>,
    pub commands: Vec<AvailableCommand>,
    /// Sessions that already exist when the agent starts.
    pub sessions: Vec<SessionRecord>,
    /// Scripted prompt turns; prompts matching none get the demo behavior.
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub load_session: bool,
    pub list: bool,
    pub resume: bool,
    pub close: bool,
    pub delete: bool,
    pub logout: bool,
    pub image: bool,
    pub audio: bool,
    pub embedded_context: bool,
    pub mcp_http: bool,
    pub mcp_sse: bool,
}

/// A stored session: scenario seeds use this shape, and so do the
/// `sessions/*.json` files in the state directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecord {
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Replayed by `session/load`.
    #[serde(default)]
    pub history: Vec<SessionUpdate>,
}

impl SessionRecord {
    /// What `session/list` requires of a `SessionInfo`: an absolute `cwd`
    /// and an ISO 8601 (RFC 3339) `updatedAt`.
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.session_id.is_empty(), "session_id must not be empty");
        anyhow::ensure!(
            self.cwd.is_absolute(),
            "cwd must be an absolute path, got {}",
            self.cwd.display()
        );
        if let Some(updated_at) = &self.updated_at {
            chrono::DateTime::parse_from_rfc3339(updated_at).with_context(|| {
                format!("updated_at must be an ISO 8601 timestamp such as 2026-01-01T00:00:00Z, got `{updated_at}`")
            })?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Turn {
    /// Substring of the prompt text; the empty string matches every prompt.
    #[serde(rename = "match")]
    pub pattern: String,
    pub steps: Vec<Step>,
    pub stop_reason: StopReason,
}

impl Default for Turn {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            steps: Vec::new(),
            stop_reason: StopReason::EndTurn,
        }
    }
}

impl Scenario {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading scenario {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing scenario {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let scenario: Self = serde_json::from_str(text)?;
        let mut ids = std::collections::BTreeSet::new();
        for record in &scenario.sessions {
            record
                .validate()
                .with_context(|| format!("invalid session `{}`", record.session_id))?;
            anyhow::ensure!(
                ids.insert(record.session_id.as_str()),
                "duplicate session id `{}`",
                record.session_id
            );
        }
        Ok(scenario)
    }

    /// The first scripted turn whose `match` occurs in `prompt`.
    pub fn find_turn(&self, prompt: &str) -> Option<&Turn> {
        self.turns
            .iter()
            .find(|turn| prompt.contains(turn.pattern.as_str()))
    }
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            agent_info: AgentInfo::default(),
            capabilities: Capabilities::default(),
            auth_methods: Vec::new(),
            require_auth: false,
            state_dir: None,
            config_options: demo_config_options(),
            modes: Some(demo_modes()),
            commands: demo_commands(),
            sessions: Vec::new(),
            turns: Vec::new(),
        }
    }
}

impl Default for AgentInfo {
    fn default() -> Self {
        Self {
            name: "acpc-mock-agent".to_string(),
            title: Some("Mock Agent".to_string()),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            load_session: true,
            list: true,
            resume: true,
            close: true,
            delete: true,
            logout: true,
            image: true,
            audio: false,
            embedded_context: true,
            mcp_http: false,
            mcp_sse: false,
        }
    }
}

/// Config option id the demo reads to skip edit permission prompts.
pub const AUTO_APPROVE_OPTION: &str = "auto_approve";
/// Mode id the demo treats like auto-approve.
pub const ACCEPT_EDITS_MODE: &str = "accept_edits";

fn demo_config_options() -> Vec<SessionConfigOption> {
    vec![
        SessionConfigOption::select(
            "model",
            "Model",
            "mock-fast",
            vec![
                SessionConfigSelectOption::new("mock-fast", "Mock Fast")
                    .description("Quick canned answers".to_string()),
                SessionConfigSelectOption::new("mock-smart", "Mock Smart")
                    .description("The same canned answers, slower to think".to_string()),
            ],
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            "reasoning",
            "Reasoning effort",
            "medium",
            vec![
                SessionConfigSelectOption::new("low", "Low"),
                SessionConfigSelectOption::new("medium", "Medium"),
                SessionConfigSelectOption::new("high", "High"),
            ],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
        SessionConfigOption::boolean(AUTO_APPROVE_OPTION, "Auto-approve edits", false)
            .description("Skip the permission prompt in the edit demo".to_string()),
    ]
}

fn demo_modes() -> SessionModeState {
    SessionModeState::new(
        "default",
        vec![
            SessionMode::new("default", "Default")
                .description("Ask before editing files".to_string()),
            SessionMode::new(ACCEPT_EDITS_MODE, "Accept edits")
                .description("Apply edits without asking".to_string()),
            SessionMode::new("plan", "Plan").description("Read-only planning".to_string()),
        ],
    )
}

fn demo_commands() -> Vec<AvailableCommand> {
    vec![
        AvailableCommand::new("help", "List the demo keywords"),
        AvailableCommand::new("echo", "Echo the given text").input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("text to echo")),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::SessionConfigKind;
    use pretty_assertions::assert_eq;

    #[test]
    fn empty_scenario_is_the_demo() {
        let scenario = Scenario::parse("{}").expect("parse");
        assert_eq!(scenario.agent_info.name, "acpc-mock-agent");
        assert!(scenario.capabilities.load_session && scenario.capabilities.list);
        assert!(!scenario.capabilities.mcp_http);
        assert_eq!(scenario.config_options.len(), 3);
        assert!(scenario.modes.is_some());
        assert_eq!(scenario.commands.len(), 2);
        assert!(scenario.turns.is_empty());
    }

    #[test]
    fn parses_full_scenario() {
        let scenario = Scenario::parse(
            r#"{
                "agent_info": {"name": "mock", "title": "Mock Agent", "version": "1.0.0"},
                "capabilities": {"load_session": false, "list": true, "mcp_http": true},
                "auth_methods": [
                    {"id": "login", "name": "Log in"},
                    {"type": "terminal", "id": "tlogin", "name": "Terminal login", "args": ["--login"], "env": {}}
                ],
                "require_auth": true,
                "state_dir": "/tmp/mock-state",
                "config_options": [
                    {"id": "model", "name": "Model", "type": "select", "currentValue": "a",
                     "options": [{"value": "a", "name": "A"}, {"value": "b", "name": "B"}]}
                ],
                "modes": null,
                "commands": [{"name": "web", "description": "Search", "input": {"hint": "query"}}],
                "sessions": [{"session_id": "s1", "cwd": "/abs", "title": "t",
                              "updated_at": "2026-01-01T00:00:00Z",
                              "history": [{"sessionUpdate": "user_message_chunk",
                                           "content": {"type": "text", "text": "hi"}}]}],
                "turns": [{"match": "hello", "steps": [{"text": "hi"}], "stop_reason": "refusal"}]
            }"#,
        )
        .expect("parse");
        assert_eq!(scenario.agent_info.title.as_deref(), Some("Mock Agent"));
        assert!(!scenario.capabilities.load_session);
        assert!(
            scenario.capabilities.resume,
            "unspecified capabilities keep defaults"
        );
        assert!(scenario.capabilities.mcp_http);
        assert_eq!(scenario.auth_methods.len(), 2);
        assert!(matches!(scenario.auth_methods[1], AuthMethod::Terminal(_)));
        assert!(scenario.require_auth);
        assert_eq!(scenario.config_options.len(), 1);
        assert!(matches!(
            scenario.config_options[0].kind,
            SessionConfigKind::Select(_)
        ));
        assert!(scenario.modes.is_none());
        assert_eq!(scenario.sessions[0].history.len(), 1);
        let turn = scenario.find_turn("say hello there").expect("turn");
        assert_eq!(turn.stop_reason, StopReason::Refusal);
        assert!(scenario.find_turn("goodbye").is_none());
    }

    #[test]
    fn first_matching_turn_wins_and_empty_match_is_catch_all() {
        let scenario = Scenario::parse(
            r#"{"turns": [
                {"match": "a", "steps": [{"text": "first"}]},
                {"match": "", "stop_reason": "max_tokens"}
            ]}"#,
        )
        .expect("parse");
        assert_eq!(scenario.find_turn("cat").map(|t| t.steps.len()), Some(1));
        assert_eq!(
            scenario.find_turn("dog").map(|t| t.stop_reason),
            Some(StopReason::MaxTokens)
        );
    }

    #[test]
    fn reports_bad_fields() {
        let err = Scenario::parse(r#"{"capabilitys": {}}"#).expect_err("typo");
        assert!(err.to_string().contains("unknown field"), "{err}");
        let err = Scenario::parse(r#"{"turns": [{"match": "x", "steps": [{"sleep": 1}]}]}"#)
            .expect_err("bad step");
        assert!(err.to_string().contains("unrecognized step"), "{err}");
        let err = Scenario::parse(r#"{"turns": [{"stop_reason": "nope"}]}"#).expect_err("reason");
        assert!(err.to_string().contains("unknown variant"), "{err}");
    }

    #[test]
    fn validates_session_seeds() {
        let seed = |fields: &str| format!(r#"{{"sessions": [{{"session_id": "s1", {fields}}}]}}"#);
        Scenario::parse(&seed(
            r#""cwd": "/abs", "updated_at": "2026-01-01T00:00:00+02:00""#,
        ))
        .expect("valid seed");
        let err = Scenario::parse(&seed(r#""cwd": "rel/dir""#)).expect_err("relative cwd");
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");
        let err = Scenario::parse(&seed(r#""cwd": "/abs", "updated_at": "yesterday""#))
            .expect_err("bad timestamp");
        assert!(format!("{err:#}").contains("ISO 8601"), "{err:#}");
        let err = Scenario::parse(
            r#"{"sessions": [{"session_id": "s1", "cwd": "/a"}, {"session_id": "s1", "cwd": "/b"}]}"#,
        )
        .expect_err("duplicate");
        assert!(err.to_string().contains("duplicate session id"), "{err}");
    }
}
