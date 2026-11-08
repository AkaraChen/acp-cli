//! The built-in demo turn, driven by keywords in the prompt so a human can
//! exercise every client feature from a TUI.

use std::collections::BTreeMap;
use std::path::Path;

use agent_client_protocol::v1::schema as acp;
use serde_json::json;

use crate::agent::now;
use crate::step::ElicitationBranches;
use crate::step::ElicitationStep;
use crate::step::FailStep;
use crate::step::Payload;
use crate::step::PermissionStep;
use crate::step::Step;
use crate::step::default_permission_options;

pub const HELP: &str = "I'm the acpc mock agent. Include any of these words in a prompt:\n\
- tool: run a command (execute tool call with streamed output)\n\
- read: read a file (tool call with a location)\n\
- edit: edit a file (diff, after asking permission)\n\
- plan: show a plan whose entries progress\n\
- ask: ask you to fill in a form (elicitation)\n\
- slow: stream a long answer you can cancel\n\
- refuse: end the turn with a refusal\n\
- error: fail the prompt with a JSON-RPC error\n";

pub struct DemoInput<'a> {
    pub prompt: &'a str,
    pub session_id: &'a acp::SessionId,
    pub cwd: &'a Path,
    /// Unique per turn; tool call ids derive from it.
    pub turn_id: &'a str,
    /// Set on the session's first prompt; announced as `session_info_update`.
    pub new_title: Option<&'a str>,
    pub form_elicitation: bool,
    pub auto_approve: bool,
    pub used_tokens: u64,
}

/// The steps and stop reason for a prompt that matched no scenario turn.
pub fn plan(input: &DemoInput<'_>) -> (Vec<Step>, acp::StopReason) {
    let prompt = input.prompt.trim();
    let lower = prompt.to_lowercase();
    let has = |keyword: &str| lower.contains(keyword);
    let mut steps = Vec::new();
    let mut stop_reason = acp::StopReason::EndTurn;

    if let Some(title) = input.new_title {
        steps.push(update(acp::SessionUpdate::SessionInfoUpdate(
            acp::SessionInfoUpdate::new().title(title).updated_at(now()),
        )));
    }
    steps.push(Step::Thought(
        format!("The user said \"{prompt}\". Let me look for demo keywords.").into(),
    ));
    if lower.starts_with("/help") {
        steps.push(text(HELP));
    } else if let Some(rest) = prompt.strip_prefix("/echo") {
        steps.push(text(rest.trim()));
    } else {
        steps.push(text(&format!("You said: {prompt}")));
    }

    let mut matched = false;
    let mut add = |keyword: &str, extra: Vec<Step>| {
        if has(keyword) {
            matched = true;
            steps.extend(extra);
        }
    };
    add("plan", plan_steps());
    add("tool", execute_steps(&format!("{}_exec", input.turn_id)));
    add(
        "read",
        read_steps(&format!("{}_read", input.turn_id), input.cwd),
    );
    add(
        "edit",
        edit_steps(
            &format!("{}_edit", input.turn_id),
            input.cwd,
            input.auto_approve,
        ),
    );
    add("ask", ask_steps(input.session_id, input.form_elicitation));
    add("slow", slow_steps());
    if has("refuse") {
        add("refuse", vec![text("\n\nSorry, I can't help with that.")]);
        stop_reason = acp::StopReason::Refusal;
    }
    add(
        "error",
        vec![Step::Fail(FailStep {
            code: -32603,
            message: "Mock agent error (the prompt asked for one)".to_string(),
            data: None,
        })],
    );
    if !matched && !lower.starts_with('/') {
        steps.push(text(
            "\n\nTry a prompt containing: tool, read, edit, plan, ask, slow, refuse or error.",
        ));
    }
    steps.push(Step::Usage(acp::UsageUpdate::new(
        input.used_tokens,
        200_000,
    )));
    (steps, stop_reason)
}

fn update(update: acp::SessionUpdate) -> Step {
    Step::Update(Box::new(Payload::Ready(update)))
}

fn text(text: &str) -> Step {
    Step::Text(Payload::Ready(text.to_string()))
}

fn sleep(millis: u64) -> Step {
    Step::Sleep(std::time::Duration::from_millis(millis))
}

fn tool_update(id: &str, fields: acp::ToolCallUpdateFields) -> Step {
    update(acp::SessionUpdate::ToolCallUpdate(
        acp::ToolCallUpdate::new(id.to_string(), fields),
    ))
}

fn status(status: acp::ToolCallStatus) -> acp::ToolCallUpdateFields {
    acp::ToolCallUpdateFields::new().status(status)
}

fn plan_steps() -> Vec<Step> {
    let tasks = [
        "Inspect the project",
        "Make the change",
        "Verify the result",
    ];
    let mut steps = Vec::new();
    for done in 0..=tasks.len() {
        let entries = tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                let status = match index.cmp(&done) {
                    std::cmp::Ordering::Less => acp::PlanEntryStatus::Completed,
                    std::cmp::Ordering::Equal => acp::PlanEntryStatus::InProgress,
                    std::cmp::Ordering::Greater => acp::PlanEntryStatus::Pending,
                };
                acp::PlanEntry::new(*task, acp::PlanEntryPriority::Medium, status)
            })
            .collect();
        if done > 0 {
            steps.push(sleep(150));
        }
        steps.push(update(acp::SessionUpdate::Plan(acp::Plan::new(entries))));
    }
    steps
}

fn execute_steps(id: &str) -> Vec<Step> {
    let call = acp::ToolCall::new(id.to_string(), "Run `ls -la`")
        .kind(acp::ToolKind::Execute)
        .status(acp::ToolCallStatus::InProgress)
        .raw_input(json!({"command": "ls -la"}));
    let first = "total 8\n";
    let full = "total 8\ndrwxr-xr-x 2 mock mock 4096 .\n-rw-r--r-- 1 mock mock  120 README.md\n";
    vec![
        update(acp::SessionUpdate::ToolCall(call)),
        sleep(150),
        tool_update(
            id,
            acp::ToolCallUpdateFields::new().content(vec![first.into()]),
        ),
        sleep(150),
        tool_update(
            id,
            acp::ToolCallUpdateFields::new().content(vec![full.into()]),
        ),
        tool_update(
            id,
            status(acp::ToolCallStatus::Completed).raw_output(json!({"exit_code": 0})),
        ),
    ]
}

fn read_steps(id: &str, cwd: &Path) -> Vec<Step> {
    let path = cwd.join("README.md");
    let call = acp::ToolCall::new(id.to_string(), "Read README.md")
        .kind(acp::ToolKind::Read)
        .status(acp::ToolCallStatus::InProgress)
        .locations(vec![acp::ToolCallLocation::new(path.clone())])
        .raw_input(json!({"path": path}));
    vec![
        update(acp::SessionUpdate::ToolCall(call)),
        sleep(100),
        tool_update(
            id,
            status(acp::ToolCallStatus::Completed)
                .content(vec!["# Demo project\n\nNothing to see here.\n".into()]),
        ),
    ]
}

fn edit_steps(id: &str, cwd: &Path, auto_approve: bool) -> Vec<Step> {
    let path = cwd.join("demo.txt");
    let diff = acp::Diff::new(path.clone(), "a\nc\n").old_text("a\nb\n");
    let title = "Edit demo.txt";
    let call = acp::ToolCall::new(id.to_string(), title)
        .kind(acp::ToolKind::Edit)
        .status(acp::ToolCallStatus::Pending)
        .content(vec![diff.into()])
        .locations(vec![acp::ToolCallLocation::new(path)]);
    let applied = vec![
        tool_update(id, status(acp::ToolCallStatus::InProgress)),
        sleep(100),
        tool_update(id, status(acp::ToolCallStatus::Completed)),
        text("\n\nApplied the edit to demo.txt."),
    ];
    let mut steps = vec![update(acp::SessionUpdate::ToolCall(call))];
    if auto_approve {
        steps.extend(applied);
        return steps;
    }
    let failed = tool_update(id, status(acp::ToolCallStatus::Failed));
    let on = BTreeMap::from([
        ("allow_once".to_string(), applied.clone()),
        ("allow_always".to_string(), applied),
        (
            "reject_once".to_string(),
            vec![failed.clone(), text("\n\nOK, I left demo.txt alone.")],
        ),
        ("cancelled".to_string(), vec![failed]),
    ]);
    let fields = acp::ToolCallUpdateFields::new()
        .title(title)
        .kind(acp::ToolKind::Edit)
        .status(acp::ToolCallStatus::Pending);
    steps.push(Step::Permission(Box::new(PermissionStep {
        tool_call: Payload::Ready(acp::ToolCallUpdate::new(id.to_string(), fields)),
        options: default_permission_options(),
        on,
    })));
    steps
}

fn ask_steps(session_id: &acp::SessionId, form_elicitation: bool) -> Vec<Step> {
    if !form_elicitation {
        return vec![text(
            "\n\n(This client did not advertise form elicitation, so I can't ask.)",
        )];
    }
    let schema = acp::ElicitationSchema::new()
        .property(
            "name",
            acp::StringPropertySchema::new().title("Your name"),
            true,
        )
        .property(
            "agree",
            acp::BooleanPropertySchema::new().title("Do you like mocks?"),
            false,
        );
    let mode = acp::ElicitationFormMode::new(
        acp::ElicitationSessionScope::new(session_id.clone()),
        schema,
    );
    let request = acp::CreateElicitationRequest::new(mode, "Tell the mock agent about yourself");
    vec![Step::Elicitation(Box::new(ElicitationStep {
        request: Payload::Ready(request),
        on: ElicitationBranches {
            accept: vec![Step::Text(Payload::Template(
                "\n\nThanks! You answered: {{elicitation}}".into(),
            ))],
            decline: vec![text("\n\nYou declined the form.")],
            cancel: vec![text("\n\nThe form was cancelled.")],
        },
    }))]
}

fn slow_steps() -> Vec<Step> {
    let mut steps = vec![text("\n\nCounting slowly:")];
    for n in 1..=50 {
        let chunk = acp::ContentChunk::new(format!(" {n}").into());
        steps.push(update(acp::SessionUpdate::AgentMessageChunk(chunk)));
        steps.push(sleep(100));
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(prompt: &'a str, session_id: &'a acp::SessionId) -> DemoInput<'a> {
        DemoInput {
            prompt,
            session_id,
            cwd: Path::new("/work"),
            turn_id: "call_1",
            new_title: None,
            form_elicitation: true,
            auto_approve: false,
            used_tokens: 10,
        }
    }

    #[test]
    fn plain_prompt_echoes_and_reports_usage() {
        let session_id = acp::SessionId::new("s");
        let (steps, stop) = plan(&input("hello", &session_id));
        assert_eq!(stop, acp::StopReason::EndTurn);
        assert!(matches!(steps.first(), Some(Step::Thought(_))));
        assert!(matches!(&steps[1], Step::Text(Payload::Ready(t)) if t == "You said: hello"));
        assert!(matches!(steps.last(), Some(Step::Usage(u)) if u.used == 10));
    }

    #[test]
    fn keywords_add_features() {
        let session_id = acp::SessionId::new("s");
        let (steps, stop) = plan(&input("edit and refuse", &session_id));
        assert_eq!(stop, acp::StopReason::Refusal);
        assert!(steps.iter().any(|step| matches!(step, Step::Permission(_))));
        let (steps, _) = plan(&DemoInput {
            auto_approve: true,
            ..input("edit", &session_id)
        });
        assert!(!steps.iter().any(|step| matches!(step, Step::Permission(_))));
        let (steps, _) = plan(&input("error", &session_id));
        assert!(steps.iter().any(|step| matches!(step, Step::Fail(_))));
        let (steps, _) = plan(&DemoInput {
            form_elicitation: false,
            ..input("ask", &session_id)
        });
        assert!(
            !steps
                .iter()
                .any(|step| matches!(step, Step::Elicitation(_)))
        );
    }
}
