//! Scripted turn steps and the `{{var}}` templating used inside them.
//!
//! A step is a JSON object identified by exactly one of the keys in
//! [`STEP_KINDS`]. ACP payloads inside steps are raw ACP JSON; they are
//! validated against the schema types when the scenario is parsed, and string
//! values may contain `{{name}}` placeholders that are filled in when the step
//! runs (see [`Vars`]).

use std::collections::BTreeMap;
use std::time::Duration;

use agent_client_protocol::v1::schema::CreateElicitationRequest;
use agent_client_protocol::v1::schema::Notice;
use agent_client_protocol::v1::schema::PermissionOption;
use agent_client_protocol::v1::schema::SessionUpdate;
use agent_client_protocol::v1::schema::ToolCallUpdate;
use agent_client_protocol::v1::schema::UsageUpdate;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Map;
use serde_json::Value;

/// Placeholder values available to templated step payloads: `prompt`,
/// `session_id`, `cwd`, and after an accepted elicitation `elicitation` (the
/// whole content as JSON) plus `elicitation.<field>`.
pub type Vars = BTreeMap<String, String>;

/// The keys that identify a step, in the order they are checked.
pub const STEP_KINDS: [&str; 10] = [
    "update",
    "sleep_ms",
    "text",
    "thought",
    "permission",
    "elicitation",
    "wait_for_cancel",
    "usage",
    "notice",
    "fail",
];

/// One scripted action inside a prompt turn.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "Map<String, Value>")]
pub enum Step {
    /// Send a raw `session/update`.
    Update(Box<Payload<SessionUpdate>>),
    /// Pause; ends the turn early if it is cancelled meanwhile.
    Sleep(Duration),
    /// Stream `agent_message_chunk`s.
    Text(Payload<String>),
    /// Stream `agent_thought_chunk`s.
    Thought(Payload<String>),
    /// Ask `session/request_permission` and branch on the outcome.
    Permission(Box<PermissionStep>),
    /// Send `elicitation/create` and branch on the action.
    Elicitation(Box<ElicitationStep>),
    /// Block until `session/cancel`, then stop with `cancelled`.
    WaitForCancel,
    /// Send a `usage_update`.
    Usage(UsageUpdate),
    /// Send a `notice` (or plain agent text if the client lacks notices).
    Notice(Notice),
    /// Answer `session/prompt` with a JSON-RPC error.
    Fail(FailStep),
}

/// `{"permission": {...}}`.
#[derive(Debug, Clone)]
pub struct PermissionStep {
    pub tool_call: Payload<ToolCallUpdate>,
    pub options: Vec<PermissionOption>,
    /// Branches keyed by option id, plus `cancelled`.
    pub on: BTreeMap<String, Vec<Step>>,
}

/// `{"elicitation": {...}, "on": {...}}`.
#[derive(Debug, Clone)]
pub struct ElicitationStep {
    pub request: Payload<CreateElicitationRequest>,
    pub on: ElicitationBranches,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ElicitationBranches {
    pub accept: Vec<Step>,
    pub decline: Vec<Step>,
    pub cancel: Vec<Step>,
}

/// `{"fail": {"code": -32603, "message": "boom", "data": ...}}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailStep {
    #[serde(default = "default_fail_code")]
    pub code: i32,
    #[serde(default = "default_fail_message")]
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

fn default_fail_code() -> i32 {
    -32603
}

fn default_fail_message() -> String {
    "mock failure".to_string()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermission {
    tool_call: Value,
    #[serde(default)]
    options: Option<Vec<PermissionOption>>,
    #[serde(default)]
    on: BTreeMap<String, Vec<Step>>,
}

impl TryFrom<Map<String, Value>> for Step {
    type Error = String;

    fn try_from(mut map: Map<String, Value>) -> Result<Self, String> {
        let Some((kind, value)) = STEP_KINDS
            .iter()
            .find_map(|kind| map.remove(*kind).map(|value| (*kind, value)))
        else {
            let keys: Vec<&String> = map.keys().collect();
            return Err(format!(
                "unrecognized step with keys {keys:?}; expected one of: {}",
                STEP_KINDS.join(", ")
            ));
        };
        let on = if kind == "elicitation" {
            map.remove("on")
        } else {
            None
        };
        if let Some(extra) = map.keys().next() {
            return Err(format!("unexpected key `{extra}` in `{kind}` step"));
        }
        parse_step(kind, value, on).map_err(|err| format!("invalid `{kind}` step: {err}"))
    }
}

fn parse_step(kind: &str, value: Value, on: Option<Value>) -> Result<Step, String> {
    let step = match kind {
        "update" => Step::Update(Box::new(Payload::from_json(value)?)),
        "sleep_ms" => Step::Sleep(Duration::from_millis(from_json(value)?)),
        "text" => Step::Text(Payload::from_json(value)?),
        "thought" => Step::Thought(Payload::from_json(value)?),
        "permission" => {
            let raw: RawPermission = from_json(value)?;
            Step::Permission(Box::new(PermissionStep {
                tool_call: Payload::from_json(raw.tool_call)?,
                options: raw.options.unwrap_or_else(default_permission_options),
                on: raw.on,
            }))
        }
        "elicitation" => Step::Elicitation(Box::new(ElicitationStep {
            request: elicitation_payload(value)?,
            on: on.map(from_json).transpose()?.unwrap_or_default(),
        })),
        "wait_for_cancel" => match value {
            Value::Bool(true) => Step::WaitForCancel,
            other => return Err(format!("expected `true`, got {other}")),
        },
        "usage" => Step::Usage(from_json(value)?),
        "notice" => Step::Notice(from_json(value)?),
        "fail" => Step::Fail(from_json(value)?),
        other => return Err(format!("unknown step kind `{other}`")),
    };
    Ok(step)
}

/// The options used when a permission step does not list any.
pub fn default_permission_options() -> Vec<PermissionOption> {
    use agent_client_protocol::v1::schema::PermissionOptionKind;
    vec![
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow_always",
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new("reject_once", "Reject", PermissionOptionKind::RejectOnce),
    ]
}

/// Elicitations default to session scope: a request with neither `sessionId`
/// nor `requestId` gets `"sessionId": "{{session_id}}"`.
fn elicitation_payload(value: Value) -> Result<Payload<CreateElicitationRequest>, String> {
    let Value::Object(mut object) = value else {
        return Err("expected an object".to_string());
    };
    if !object.contains_key("sessionId") && !object.contains_key("requestId") {
        object.insert(
            "sessionId".to_string(),
            Value::String("{{session_id}}".to_string()),
        );
    }
    Payload::from_json(Value::Object(object))
}

fn from_json<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|err| err.to_string())
}

/// A schema value that may contain `{{var}}` placeholders in its strings.
#[derive(Debug, Clone)]
pub enum Payload<T> {
    /// Fully known up front.
    Ready(T),
    /// Raw JSON whose strings are expanded before deserializing.
    Template(Value),
}

impl<T: DeserializeOwned + Clone> Payload<T> {
    /// Validates `value` against `T` and keeps it as a template if needed.
    pub fn from_json(value: Value) -> Result<Self, String> {
        let parsed = T::deserialize(&value).map_err(|err| err.to_string())?;
        if has_placeholder(&value) {
            Ok(Self::Template(value))
        } else {
            Ok(Self::Ready(parsed))
        }
    }

    pub fn render(&self, vars: &Vars) -> Result<T, serde_json::Error> {
        match self {
            Self::Ready(value) => Ok(value.clone()),
            Self::Template(raw) => {
                let mut value = raw.clone();
                expand_value(&mut value, vars);
                serde_json::from_value(value)
            }
        }
    }
}

impl<T> From<T> for Payload<T> {
    fn from(value: T) -> Self {
        Self::Ready(value)
    }
}

fn has_placeholder(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("{{"),
        Value::Array(items) => items.iter().any(has_placeholder),
        Value::Object(map) => map.values().any(has_placeholder),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn expand_value(value: &mut Value, vars: &Vars) {
    match value {
        Value::String(text) => {
            if text.contains("{{") {
                *text = expand(text, vars);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| expand_value(item, vars)),
        Value::Object(map) => map.values_mut().for_each(|item| expand_value(item, vars)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Replaces `{{name}}` with `vars[name]`; unknown placeholders stay verbatim.
pub fn expand(text: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        match vars.get(after[..end].trim()) {
            Some(replacement) => out.push_str(replacement),
            None => out.push_str(&rest[start..start + end + 4]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::ElicitationScope;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn step(value: Value) -> Result<Step, String> {
        serde_json::from_value(value).map_err(|err| err.to_string())
    }

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn expands_known_placeholders_only() {
        let vars = vars(&[("cwd", "/w"), ("elicitation.name", "Ada")]);
        assert_eq!(
            expand("{{cwd}}/a {{ elicitation.name }}", &vars),
            "/w/a Ada"
        );
        assert_eq!(expand("{{nope}} {{cwd", &vars), "{{nope}} {{cwd");
        assert_eq!(expand("plain", &vars), "plain");
    }

    #[test]
    fn parses_simple_steps() {
        assert!(matches!(step(json!({"sleep_ms": 5})), Ok(Step::Sleep(d)) if d.as_millis() == 5));
        assert!(matches!(
            step(json!({"wait_for_cancel": true})),
            Ok(Step::WaitForCancel)
        ));
        assert!(matches!(
            step(json!({"text": "hi"})),
            Ok(Step::Text(Payload::Ready(text))) if text == "hi"
        ));
        assert!(matches!(
            step(json!({"usage": {"used": 1, "size": 2}})),
            Ok(Step::Usage(usage)) if usage.used == 1 && usage.size == 2
        ));
        assert!(matches!(
            step(json!({"fail": {"message": "boom"}})),
            Ok(Step::Fail(fail)) if fail.code == -32603 && fail.message == "boom"
        ));
        assert!(matches!(
            step(json!({"notice": {"severity": "warning", "title": "careful"}})),
            Ok(Step::Notice(notice)) if notice.title == "careful"
        ));
    }

    #[test]
    fn update_payloads_are_validated_and_templated() {
        let parsed = step(json!({"update": {
            "sessionUpdate": "tool_call",
            "toolCallId": "c1",
            "title": "Read",
            "locations": [{"path": "{{cwd}}/README.md"}]
        }}))
        .expect("valid update");
        let Step::Update(payload) = parsed else {
            panic!("expected update step");
        };
        let rendered = payload.render(&vars(&[("cwd", "/w")])).expect("render");
        let SessionUpdate::ToolCall(call) = rendered else {
            panic!("expected tool_call");
        };
        assert_eq!(call.locations[0].path.to_string_lossy(), "/w/README.md");

        let err = step(json!({"update": {"sessionUpdate": "bogus"}})).expect_err("invalid");
        assert!(err.contains("invalid `update` step"), "{err}");
    }

    #[test]
    fn permission_steps_have_branches_and_default_options() {
        let parsed = step(json!({"permission": {
            "tool_call": {"toolCallId": "c1"},
            "on": {"allow_once": [{"text": "ok"}], "cancelled": []}
        }}))
        .expect("valid permission");
        let Step::Permission(permission) = parsed else {
            panic!("expected permission step");
        };
        assert_eq!(permission.options.len(), 3);
        assert_eq!(
            permission.on.keys().collect::<Vec<_>>(),
            vec!["allow_once", "cancelled"]
        );
    }

    #[test]
    fn elicitation_defaults_to_session_scope() {
        let parsed = step(json!({
            "elicitation": {
                "mode": "form",
                "message": "Who?",
                "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}
            },
            "on": {"accept": [{"text": "hi {{elicitation.name}}"}]}
        }))
        .expect("valid elicitation");
        let Step::Elicitation(elicitation) = parsed else {
            panic!("expected elicitation step");
        };
        assert_eq!(elicitation.on.accept.len(), 1);
        let request = elicitation
            .request
            .render(&vars(&[("session_id", "s1")]))
            .expect("render");
        let ElicitationScope::Session(scope) = request.scope() else {
            panic!("expected session scope");
        };
        assert_eq!(scope.session_id.to_string(), "s1");
    }

    #[test]
    fn rejects_unknown_and_extra_keys() {
        let err = step(json!({"bogus": 1})).expect_err("unknown");
        assert!(err.contains("unrecognized step"), "{err}");
        let err = step(json!({"text": "a", "on": {}})).expect_err("extra");
        assert!(err.contains("unexpected key `on`"), "{err}");
        let err = step(json!({"wait_for_cancel": false})).expect_err("false");
        assert!(err.contains("expected `true`"), "{err}");
    }
}
