//! Plain-terminal answers to agent elicitations that arrive while acpc
//! signs in, before the TUI owns the terminal (elicitation.mdx).

use std::collections::BTreeMap;
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
    let line = read_line(&format!("{label}{marker}: "))?;
    let text = line.trim();
    if text == ":cancel" {
        return None;
    }
    if text.is_empty() {
        return Some(Answer::Skip);
    }
    Some(match property {
        acp::ElicitationPropertySchema::String(_) if !choices.is_empty() => {
            match pick(text, &choices) {
                Some(value) => Answer::Value(acp::ElicitationContentValue::String(value)),
                None => Answer::Invalid("Pick one of the listed numbers.".to_string()),
            }
        }
        acp::ElicitationPropertySchema::String(_) => {
            Answer::Value(acp::ElicitationContentValue::String(text.to_string()))
        }
        acp::ElicitationPropertySchema::Integer(_) => match text.parse::<i64>() {
            Ok(value) => Answer::Value(acp::ElicitationContentValue::Integer(value)),
            Err(_) => Answer::Invalid("Enter a whole number.".to_string()),
        },
        acp::ElicitationPropertySchema::Number(_) => match text.parse::<f64>() {
            Ok(value) if value.is_finite() => {
                Answer::Value(acp::ElicitationContentValue::Number(value))
            }
            _ => Answer::Invalid("Enter a number.".to_string()),
        },
        acp::ElicitationPropertySchema::Boolean(_) => match text.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" => Answer::Value(acp::ElicitationContentValue::Boolean(true)),
            "n" | "no" | "false" => Answer::Value(acp::ElicitationContentValue::Boolean(false)),
            _ => Answer::Invalid("Answer y or n.".to_string()),
        },
        acp::ElicitationPropertySchema::Array(_) => {
            let picked: Option<Vec<String>> = text
                .split(',')
                .map(|part| pick(part.trim(), &choices))
                .collect();
            match picked {
                Some(values) => Answer::Value(acp::ElicitationContentValue::StringArray(values)),
                None => Answer::Invalid("List numbers from above, separated by commas.".to_string()),
            }
        }
        _ => Answer::Invalid("This field cannot be answered here.".to_string()),
    })
}

fn pick(text: &str, choices: &[(String, String)]) -> Option<String> {
    let index = text.parse::<usize>().ok()?.checked_sub(1)?;
    choices.get(index).map(|(value, _)| value.clone())
}

fn labels<'a>(name: &'a str, property: &'a acp::ElicitationPropertySchema) -> (&'a str, Option<&'a str>) {
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
