//! Conversions between Codex user input and ACP content blocks.

use std::path::Path;

use acp_client::AgentCaps;
use acp_client::schema as acp;
use base64::Engine;
use codex_app_server_protocol::UserInput;

/// Translate a Codex turn input into an ACP prompt, honouring the agent's
/// prompt capabilities. Unsupported attachments become text notes rather
/// than being sent against the spec.
pub(crate) fn prompt_blocks(
    input: &[UserInput],
    caps: &AgentCaps,
    cwd: &Path,
) -> Vec<acp::ContentBlock> {
    let mut blocks = Vec::new();
    for item in input {
        match item {
            UserInput::Text { text, .. } => {
                if !text.is_empty() {
                    blocks.push(acp::ContentBlock::Text(acp::TextContent::new(text.clone())));
                }
            }
            UserInput::LocalImage { path, .. } => {
                let path = absolute(cwd, path);
                match image_block(&path, caps) {
                    Some(block) => blocks.push(block),
                    None => blocks.push(link_block(&path)),
                }
            }
            UserInput::Image { .. } => {
                if let Some(url) = image_url(item) {
                    blocks.push(acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                        "image", url,
                    )));
                }
            }
            UserInput::Mention { path, .. } => {
                blocks.push(link_block(&absolute(cwd, Path::new(path))));
            }
            UserInput::Skill { path, .. } => {
                blocks.push(link_block(&absolute(cwd, path)));
            }
            UserInput::LocalAudio { path } => {
                blocks.push(link_block(&absolute(cwd, path)));
            }
            UserInput::Audio { .. } => {}
        }
    }
    blocks
}

fn image_url(item: &UserInput) -> Option<String> {
    let value = serde_json::to_value(item).ok()?;
    value
        .get("url")
        .or_else(|| value.get("imageUrl"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn absolute(cwd: &Path, path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn link_block(path: &Path) -> acp::ContentBlock {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    acp::ContentBlock::ResourceLink(acp::ResourceLink::new(name, file_uri(path)))
}

pub(crate) fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn image_block(path: &Path, caps: &AgentCaps) -> Option<acp::ContentBlock> {
    if !caps.prompt_image() {
        return None;
    }
    let mime_type = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return None,
    };
    let bytes = std::fs::read(path).ok()?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(acp::ContentBlock::Image(
        acp::ImageContent::new(data, mime_type).uri(file_uri(path)),
    ))
}

/// Text to display for an ACP content block.
pub(crate) fn display_text(block: &acp::ContentBlock) -> String {
    match block {
        acp::ContentBlock::Text(text) => text.text.clone(),
        acp::ContentBlock::ResourceLink(link) => {
            let label = link.title.as_deref().unwrap_or(&link.name);
            format!("[{label}]({})", link.uri)
        }
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => text.text.clone(),
            acp::EmbeddedResourceResource::BlobResourceContents(blob) => {
                format!("[resource]({})", blob.uri)
            }
            _ => String::new(),
        },
        acp::ContentBlock::Image(image) => match &image.uri {
            Some(uri) => format!("[image]({uri})"),
            None => "[image]".to_string(),
        },
        acp::ContentBlock::Audio(_) => "[audio]".to_string(),
        _ => String::new(),
    }
}

/// A user message as Codex input, for replayed history.
pub(crate) fn user_input(block: &acp::ContentBlock) -> Option<UserInput> {
    if crate::handoff::is_handoff(block) {
        return None;
    }
    match block {
        acp::ContentBlock::Text(text) => {
            crate::handoff::strip(&text.text).map(|text| UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            })
        }
        acp::ContentBlock::ResourceLink(link) => {
            let path = link.uri.strip_prefix("file://").unwrap_or(&link.uri);
            Some(UserInput::Mention {
                name: link.name.clone(),
                path: path.to_string(),
            })
        }
        acp::ContentBlock::Image(image) => image
            .uri
            .as_deref()
            .and_then(|uri| uri.strip_prefix("file://"))
            .map(|path| UserInput::LocalImage {
                detail: None,
                path: path.into(),
            }),
        other => {
            let text = display_text(other);
            (!text.is_empty()).then_some(UserInput::Text {
                text,
                text_elements: Vec::new(),
            })
        }
    }
}
