//! ACP session config options presented as Codex models and efforts.
//!
//! session-config-options.mdx: a `select` option with `category: "model"`
//! is the model selector; `thought_level` maps onto reasoning effort. When
//! an agent exposes no model option, one synthetic entry stands for "the
//! agent's own default" so the TUI always has a non-empty catalog.

use acp_client::schema as acp;
use codex_app_server_protocol::Model;
use codex_protocol::openai_models::ReasoningEffort;

use crate::records;
use crate::records::ModelEntry;

/// Model id reported when the agent exposes no model selector.
pub(crate) const AGENT_DEFAULT_MODEL: &str = "agent-default";

pub(crate) fn option_by_category<'a>(
    options: &'a [acp::SessionConfigOption],
    category: &acp::SessionConfigOptionCategory,
) -> Option<&'a acp::SessionConfigOption> {
    options.iter().find(|option| {
        option.category.as_ref() == Some(category)
            && matches!(option.kind, acp::SessionConfigKind::Select(_))
    })
}

pub(crate) fn select_values(option: &acp::SessionConfigOption) -> Vec<&acp::SessionConfigSelectOption> {
    match &option.kind {
        acp::SessionConfigKind::Select(select) => match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
            acp::SessionConfigSelectOptions::Grouped(groups) => {
                groups.iter().flat_map(|group| group.options.iter()).collect()
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

pub(crate) fn current_value(option: &acp::SessionConfigOption) -> Option<String> {
    match &option.kind {
        acp::SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
        _ => None,
    }
}

/// The model id the TUI should show as current.
pub(crate) fn current_model(options: &[acp::SessionConfigOption]) -> String {
    option_by_category(options, &acp::SessionConfigOptionCategory::Model)
        .and_then(current_value)
        .unwrap_or_else(|| AGENT_DEFAULT_MODEL.to_string())
}

/// Map an ACP thought-level value onto Codex's effort enum. Unknown values
/// survive as `Custom` so they round-trip back to the agent unchanged.
pub(crate) fn effort_from_value(value: &str) -> ReasoningEffort {
    match value.to_ascii_lowercase().as_str() {
        "none" | "off" => ReasoningEffort::None,
        "minimal" => ReasoningEffort::Minimal,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" | "x-high" | "extra-high" => ReasoningEffort::XHigh,
        "max" => ReasoningEffort::Max,
        _ => ReasoningEffort::Custom(value.to_string()),
    }
}

/// The ACP thought-level value for a Codex effort, if the agent offers it.
pub(crate) fn effort_value(
    options: &[acp::SessionConfigOption],
    effort: &ReasoningEffort,
) -> Option<(acp::SessionConfigId, String)> {
    let option = option_by_category(options, &acp::SessionConfigOptionCategory::ThoughtLevel)?;
    select_values(option)
        .into_iter()
        .find(|value| &effort_from_value(&value.value.to_string()) == effort)
        .map(|value| (option.id.clone(), value.value.to_string()))
}

pub(crate) fn current_effort(options: &[acp::SessionConfigOption]) -> Option<ReasoningEffort> {
    option_by_category(options, &acp::SessionConfigOptionCategory::ThoughtLevel)
        .and_then(current_value)
        .map(|value| effort_from_value(&value))
}

fn efforts(options: &[acp::SessionConfigOption]) -> Vec<(ReasoningEffort, String)> {
    option_by_category(options, &acp::SessionConfigOptionCategory::ThoughtLevel)
        .map(|option| {
            select_values(option)
                .into_iter()
                .map(|value| {
                    (
                        effort_from_value(&value.value.to_string()),
                        value.description.clone().unwrap_or_else(|| value.name.clone()),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `model/list` catalog for a session's config options.
pub(crate) fn catalog(
    options: &[acp::SessionConfigOption],
    agent_name: &str,
    image_input: bool,
) -> Vec<Model> {
    let efforts = efforts(options);
    let default_effort = current_effort(options);
    match option_by_category(options, &acp::SessionConfigOptionCategory::Model) {
        Some(option) => {
            let current = current_value(option);
            select_values(option)
                .into_iter()
                .map(|value| {
                    let id = value.value.to_string();
                    records::model(ModelEntry {
                        is_default: current.as_deref() == Some(id.as_str()),
                        id: &id,
                        display_name: &value.name,
                        description: value.description.as_deref().unwrap_or(""),
                        efforts: efforts.clone(),
                        default_effort: default_effort.clone(),
                        image_input,
                    })
                })
                .collect()
        }
        None => vec![records::model(ModelEntry {
            id: AGENT_DEFAULT_MODEL,
            display_name: agent_name,
            description: "The agent's configured model",
            is_default: true,
            efforts,
            default_effort,
            image_input,
        })],
    }
}

/// The config option and value that select `model`, if the agent offers it.
pub(crate) fn model_value(
    options: &[acp::SessionConfigOption],
    model: &str,
) -> Option<(acp::SessionConfigId, String)> {
    let option = option_by_category(options, &acp::SessionConfigOptionCategory::Model)?;
    select_values(option)
        .into_iter()
        .find(|value| value.value.to_string() == model)
        .map(|value| (option.id.clone(), value.value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn select(id: &str, category: acp::SessionConfigOptionCategory, values: &[&str], current: &str) -> acp::SessionConfigOption {
        acp::SessionConfigOption::select(
            id.to_string(),
            id.to_string(),
            current.to_string(),
            values
                .iter()
                .map(|value| acp::SessionConfigSelectOption::new(value.to_string(), value.to_uppercase()))
                .collect::<Vec<_>>(),
        )
        .category(category)
    }

    #[test]
    fn catalog_lists_agent_models_with_efforts() {
        let options = vec![
            select("model", acp::SessionConfigOptionCategory::Model, &["sonnet", "opus"], "opus"),
            select("effort", acp::SessionConfigOptionCategory::ThoughtLevel, &["low", "high", "ultra"], "high"),
        ];
        let models = catalog(&options, "Claude", false);
        assert_eq!(
            models.iter().map(|m| (m.id.clone(), m.is_default)).collect::<Vec<_>>(),
            vec![("sonnet".to_string(), false), ("opus".to_string(), true)]
        );
        assert_eq!(
            models[0]
                .supported_reasoning_efforts
                .iter()
                .map(|e| e.reasoning_effort.clone())
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Custom("ultra".to_string())
            ]
        );
        assert_eq!(current_model(&options), "opus");
        assert_eq!(
            effort_value(&options, &ReasoningEffort::Custom("ultra".to_string())).map(|(_, v)| v),
            Some("ultra".to_string())
        );
    }

    #[test]
    fn catalog_without_model_option_has_one_default_entry() {
        let models = catalog(&[], "Agent", true);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, AGENT_DEFAULT_MODEL);
        assert!(models[0].is_default);
    }
}
