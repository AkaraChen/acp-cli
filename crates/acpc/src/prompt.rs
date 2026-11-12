//! Plain-terminal answers to agent elicitations that arrive while acpc
//! signs in, before the TUI owns the terminal (elicitation.mdx).

use std::collections::BTreeMap;
use std::fmt::Display;
use std::io::BufRead;
use std::io::Write;

use acp_client::schema as acp;

pub async fn elicit(request: acp::CreateElicitationRequest) -> acp::CreateElicitationResponse {
    tokio::task::spawn_blocking(move || elicit_blocking(&request))
        .await
        .unwrap_or_else(|_| cancel())
}

fn cancel() -> acp::CreateElicitationResponse {
    acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel)
}

fn read_line(prompt: &str) -> Option<String> {
    eprint!("{prompt}");
    std::io::stderr().flush().ok()?;
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line).ok()?;
    (read > 0).then(|| line.trim_end_matches(['\n', '\r']).to_string())
}

fn elicit_blocking(request: &acp::CreateElicitationRequest) -> acp::CreateElicitationResponse {
    eprintln!("\n{}", request.message);
    match &request.mode {
        acp::ElicitationMode::Form(form) => form_answer(&form.requested_schema),
        acp::ElicitationMode::Url(url) => {
            // Show the full URL; never open it without the user.
            eprintln!("Open this link to continue:\n  {}", url.url);
            match read_line("Press Enter when done, or type n to cancel: ") {
                Some(answer) if !answer.trim().eq_ignore_ascii_case("n") => {
                    acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                        acp::ElicitationAcceptAction::new(),
                    ))
                }
                _ => cancel(),
            }
        }
        _ => cancel(),
    }
}

fn form_answer(schema: &acp::ElicitationSchema) -> acp::CreateElicitationResponse {
    eprintln!("(Enter a value for each field; type :cancel to stop.)");
    let required = schema.required.clone().unwrap_or_default();
    let mut content = BTreeMap::new();
    for (name, property) in &schema.properties {
        let is_required = required.contains(name);
        loop {
            let Some(value) = ask(name, property, is_required) else {
                return cancel();
            };
            match value {
                Answer::Value(value) => {
                    content.insert(name.clone(), value);
                    break;
                }
                Answer::Skip if !is_required => break,
                Answer::Skip => eprintln!("  {name} is required."),
                Answer::Invalid(reason) => eprintln!("  {reason}"),
            }
        }
    }
    acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
        acp::ElicitationAcceptAction::new().content(Some(content)),
    ))
}

enum Answer {
    Value(acp::ElicitationContentValue),
    Skip,
    Invalid(String),
}

/// `None` means the user cancelled the whole form.
fn ask(name: &str, property: &acp::ElicitationPropertySchema, required: bool) -> Option<Answer> {
    let marker = if required { "*" } else { "" };
    let (label, description) = labels(name, property);
    if let Some(description) = description {
        eprintln!("  {description}");
    }
    let choices = choices(property);
    if !choices.is_empty() {
        for (index, (_, title)) in choices.iter().enumerate() {
            eprintln!("    {}. {title}", index + 1);
        }
    }
    let default = default_value(property);
    let shown = default
        .as_ref()
        .map(|value| format!(" [{}]", show(value, &choices)))
        .unwrap_or_default();
    let line = read_line(&format!("{label}{marker}{shown}: "))?;
    let text = line.trim();
    if text == ":cancel" {
        return None;
    }
    if text.is_empty() {
        return Some(default.map_or(Answer::Skip, Answer::Value));
    }
    Some(parse(text, property, &choices))
}

fn parse(
    text: &str,
    property: &acp::ElicitationPropertySchema,
    choices: &[(String, String)],
) -> Answer {
    match property {
        acp::ElicitationPropertySchema::String(_) if !choices.is_empty() => {
            match pick(text, choices) {
                Some(value) => Answer::Value(acp::ElicitationContentValue::String(value)),
                None => Answer::Invalid("Pick one of the listed numbers.".to_string()),
            }
        }
        acp::ElicitationPropertySchema::String(schema) => {
            let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
            match check(
                length,
                schema.min_length.map(u64::from),
                schema.max_length.map(u64::from),
            ) {
                Ok(()) => Answer::Value(acp::ElicitationContentValue::String(text.to_string())),
                Err(bounds) => Answer::Invalid(format!("Use {bounds} characters.")),
            }
        }
        acp::ElicitationPropertySchema::Integer(schema) => match text.parse::<i64>() {
            Ok(value) => match check(value, schema.minimum, schema.maximum) {
                Ok(()) => Answer::Value(acp::ElicitationContentValue::Integer(value)),
                Err(bounds) => Answer::Invalid(format!("Enter a whole number, {bounds}.")),
            },
            Err(_) => Answer::Invalid("Enter a whole number.".to_string()),
        },
        acp::ElicitationPropertySchema::Number(schema) => match text.parse::<f64>() {
            Ok(value) if value.is_finite() => match check(value, schema.minimum, schema.maximum) {
                Ok(()) => Answer::Value(acp::ElicitationContentValue::Number(value)),
                Err(bounds) => Answer::Invalid(format!("Enter a number, {bounds}.")),
            },
            _ => Answer::Invalid("Enter a number.".to_string()),
        },
        acp::ElicitationPropertySchema::Boolean(_) => match text.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" => Answer::Value(acp::ElicitationContentValue::Boolean(true)),
            "n" | "no" | "false" => Answer::Value(acp::ElicitationContentValue::Boolean(false)),
            _ => Answer::Invalid("Answer y or n.".to_string()),
        },
        acp::ElicitationPropertySchema::Array(schema) => {
            let picked: Option<Vec<String>> = text
                .split(',')
                .map(|part| pick(part.trim(), choices))
                .collect();
            let Some(values) = picked else {
                return Answer::Invalid(
                    "List numbers from above, separated by commas.".to_string(),
                );
            };
            let count = u64::try_from(values.len()).unwrap_or(u64::MAX);
            match check(count, schema.min_items, schema.max_items) {
                Ok(()) => Answer::Value(acp::ElicitationContentValue::StringArray(values)),
                Err(bounds) => Answer::Invalid(format!("Pick {bounds} of the listed numbers.")),
            }
        }
        _ => Answer::Invalid("This field cannot be answered here.".to_string()),
    }
}

/// `Err` describes the allowed range when `value` falls outside it.
fn check<T: PartialOrd + Display>(value: T, min: Option<T>, max: Option<T>) -> Result<(), String> {
    let below = min.as_ref().is_some_and(|min| value < *min);
    let above = max.as_ref().is_some_and(|max| value > *max);
    if !below && !above {
        return Ok(());
    }
    Err(match (min, max) {
        (Some(min), Some(max)) => format!("{min} to {max}"),
        (Some(min), None) => format!("at least {min}"),
        (None, Some(max)) => format!("at most {max}"),
        (None, None) => String::new(),
    })
}

fn default_value(
    property: &acp::ElicitationPropertySchema,
) -> Option<acp::ElicitationContentValue> {
    match property {
        acp::ElicitationPropertySchema::String(schema) => schema
            .default
            .clone()
            .map(acp::ElicitationContentValue::String),
        acp::ElicitationPropertySchema::Integer(schema) => {
            schema.default.map(acp::ElicitationContentValue::Integer)
        }
        acp::ElicitationPropertySchema::Number(schema) => {
            schema.default.map(acp::ElicitationContentValue::Number)
        }
        acp::ElicitationPropertySchema::Boolean(schema) => {
            schema.default.map(acp::ElicitationContentValue::Boolean)
        }
        acp::ElicitationPropertySchema::Array(schema) => schema
            .default
            .clone()
            .map(acp::ElicitationContentValue::StringArray),
        _ => None,
    }
}

/// A value as the user would recognise it: choices by their title.
fn show(value: &acp::ElicitationContentValue, choices: &[(String, String)]) -> String {
    let title = |value: &String| {
        choices
            .iter()
            .find(|(choice, _)| choice == value)
            .map_or_else(|| value.clone(), |(_, title)| title.clone())
    };
    match value {
        acp::ElicitationContentValue::String(value) => title(value),
        acp::ElicitationContentValue::Integer(value) => value.to_string(),
        acp::ElicitationContentValue::Number(value) => value.to_string(),
        acp::ElicitationContentValue::Boolean(true) => "y".to_string(),
        acp::ElicitationContentValue::Boolean(false) => "n".to_string(),
        acp::ElicitationContentValue::StringArray(values) => {
            values.iter().map(title).collect::<Vec<_>>().join(", ")
        }
        _ => String::new(),
    }
}

fn pick(text: &str, choices: &[(String, String)]) -> Option<String> {
    let index = text.parse::<usize>().ok()?.checked_sub(1)?;
    choices.get(index).map(|(value, _)| value.clone())
}

fn labels<'a>(
    name: &'a str,
    property: &'a acp::ElicitationPropertySchema,
) -> (&'a str, Option<&'a str>) {
    let (title, description) = match property {
        acp::ElicitationPropertySchema::String(schema) => (&schema.title, &schema.description),
        acp::ElicitationPropertySchema::Number(schema) => (&schema.title, &schema.description),
        acp::ElicitationPropertySchema::Integer(schema) => (&schema.title, &schema.description),
        acp::ElicitationPropertySchema::Boolean(schema) => (&schema.title, &schema.description),
        acp::ElicitationPropertySchema::Array(schema) => (&schema.title, &schema.description),
        _ => return (name, None),
    };
    (title.as_deref().unwrap_or(name), description.as_deref())
}

/// (value, title) pairs for enum-like fields.
fn choices(property: &acp::ElicitationPropertySchema) -> Vec<(String, String)> {
    match property {
        acp::ElicitationPropertySchema::String(schema) => {
            if let Some(options) = &schema.one_of {
                return options
                    .iter()
                    .map(|option| (option.value.clone(), option.title.clone()))
                    .collect();
            }
            schema
                .enum_values
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|value| (value.clone(), value))
                .collect()
        }
        acp::ElicitationPropertySchema::Array(schema) => match &schema.items {
            acp::MultiSelectItems::String(items) => items
                .values
                .iter()
                .map(|value| (value.clone(), value.clone()))
                .collect(),
            acp::MultiSelectItems::Titled(items) => items
                .options
                .iter()
                .map(|option| (option.value.clone(), option.title.clone()))
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn accepted(
        text: &str,
        property: &acp::ElicitationPropertySchema,
    ) -> Option<acp::ElicitationContentValue> {
        match parse(text, property, &choices(property)) {
            Answer::Value(value) => Some(value),
            Answer::Skip | Answer::Invalid(_) => None,
        }
    }

    #[test]
    fn enforces_schema_bounds() {
        let integer = acp::ElicitationPropertySchema::Integer(
            acp::IntegerPropertySchema::new().minimum(1).maximum(10),
        );
        assert_eq!(accepted("0", &integer), None);
        assert_eq!(
            accepted("10", &integer),
            Some(acp::ElicitationContentValue::Integer(10))
        );
        let number =
            acp::ElicitationPropertySchema::Number(acp::NumberPropertySchema::new().minimum(0.5));
        assert_eq!(accepted("0.25", &number), None);
        let text = acp::ElicitationPropertySchema::String(
            acp::StringPropertySchema::new().min_length(2).max_length(3),
        );
        assert_eq!(accepted("a", &text), None);
        assert_eq!(accepted("abcd", &text), None);
        assert_eq!(
            accepted("äöü", &text),
            Some(acp::ElicitationContentValue::String("äöü".to_string()))
        );
        let multi = acp::ElicitationPropertySchema::Array(
            acp::MultiSelectPropertySchema::new(vec!["a".to_string(), "b".to_string()])
                .max_items(1),
        );
        assert_eq!(accepted("1,2", &multi), None);
        assert_eq!(
            accepted("2", &multi),
            Some(acp::ElicitationContentValue::StringArray(vec![
                "b".to_string()
            ]))
        );
    }

    #[test]
    fn shows_defaults_as_the_user_sees_them() {
        let multi = acp::ElicitationPropertySchema::Array(
            acp::MultiSelectPropertySchema::titled(vec![
                acp::EnumOption::new("a", "Alpha"),
                acp::EnumOption::new("b", "Beta"),
            ])
            .default_value(vec!["b".to_string()]),
        );
        let default = default_value(&multi);
        assert_eq!(
            default,
            Some(acp::ElicitationContentValue::StringArray(vec![
                "b".to_string()
            ]))
        );
        assert_eq!(
            default.map(|value| show(&value, &choices(&multi))),
            Some("Beta".to_string())
        );
        let boolean = acp::ElicitationPropertySchema::Boolean(
            acp::BooleanPropertySchema::new().default_value(true),
        );
        assert_eq!(
            default_value(&boolean).map(|value| show(&value, &[])),
            Some("y".to_string())
        );
    }
}
