//! Utility functions for the agent

use agent_client_protocol::{ContentBlock, EmbeddedResourceResource, ToolCallLocation, ToolKind};

/// Format only user text from prompt blocks (first Text block only).
/// This excludes attachment content which is in subsequent Text blocks.
/// Used for intent snapshots and session titles where we want clean user text.
pub fn format_prompt_user_text_only(blocks: &[ContentBlock]) -> String {
    blocks
        .first()
        .and_then(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Formats prompt content blocks into a single string
pub fn format_prompt_blocks(blocks: &[ContentBlock], max_prompt_bytes: Option<usize>) -> String {
    let mut content = String::new();
    for block in blocks {
        if !content.is_empty() {
            content.push_str("\n\n");
        }
        match block {
            ContentBlock::Text(text) => {
                content.push_str(&text.text);
            }
            ContentBlock::ResourceLink(link) => {
                content.push_str(&format!(
                    "[Resource: {}] {}\n{}",
                    link.name,
                    link.uri,
                    link.description.clone().unwrap_or_default()
                ));
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    content.push_str(&format!("[Embedded Resource: {}]\n{}", text.uri, text.text));
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    content.push_str(&format!(
                        "[Embedded Resource: {}] (blob, {} bytes)",
                        blob.uri,
                        blob.blob.len()
                    ));
                }
                _ => {
                    content.push_str("[Embedded Resource: unsupported]");
                }
            },
            ContentBlock::Image(image) => {
                content.push_str(&format!(
                    "[Image] mime={}, bytes={}",
                    image.mime_type,
                    image.data.len()
                ));
            }
            ContentBlock::Audio(audio) => {
                content.push_str(&format!(
                    "[Audio] mime={}, bytes={}",
                    audio.mime_type,
                    audio.data.len()
                ));
            }
            _ => {
                content.push_str("[Unsupported content block]");
            }
        }
    }

    if let Some(max_bytes) = max_prompt_bytes {
        truncate_to_bytes(&content, max_bytes)
    } else {
        content
    }
}

/// Approximates token count based on character count
pub fn approximate_token_count(messages: &[querymt::chat::ChatMessage]) -> usize {
    let mut chars = 0usize;
    for msg in messages {
        chars += msg.content.len();
    }
    (chars / 4).max(1)
}

/// Truncates a string to fit within a byte limit
pub fn truncate_to_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let note = "\n[truncated]";
    if max_bytes <= note.len() {
        return note[..max_bytes].to_string();
    }

    let mut end = max_bytes - note.len();
    while !input.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut truncated = input[..end].to_string();
    truncated.push_str(note);
    truncated
}

/// Determines the tool kind for a given tool name
pub fn tool_kind_for_tool(name: &str) -> ToolKind {
    match name {
        "search_text" => ToolKind::Search,
        "write_file" | "apply_patch" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "shell" => ToolKind::Execute,
        "web_fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Extracts file paths from tool arguments for location tracking
pub fn extract_locations(args: &serde_json::Value) -> Vec<ToolCallLocation> {
    let mut locations = Vec::new();
    let Some(map) = args.as_object() else {
        return locations;
    };
    if let Some(path) = map.get("path").and_then(|v| v.as_str()) {
        locations.push(ToolCallLocation::new(path));
    }
    if let Some(root) = map.get("root").and_then(|v| v.as_str()) {
        locations.push(ToolCallLocation::new(root));
    }
    locations
}
