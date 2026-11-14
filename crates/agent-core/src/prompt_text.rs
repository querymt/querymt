//! Pure prompt-text helpers: rendering `ContentBlock`s for display and
//! truncating text to a byte budget.

use crate::protocol::ContentBlock;

const ATTACHMENTS_DISPLAY_FALLBACK: &str = "(attachments included)";
const IMAGE_DISPLAY_FALLBACK: &str = "(image attached)";

/// Collect user-authored text without incorporating attachment payloads.
pub fn format_prompt_user_text_only(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render prompt blocks for user-facing displays/events.
///
/// If there is no user text but attachments exist, return a placeholder.
pub fn render_prompt_for_display(blocks: &[ContentBlock]) -> String {
    let user_text = format_prompt_user_text_only(blocks).trim().to_string();
    if !user_text.is_empty() {
        return user_text;
    }

    if crate::model::prompt_contains_images(blocks) {
        IMAGE_DISPLAY_FALLBACK.to_string()
    } else if !blocks.is_empty() {
        ATTACHMENTS_DISPLAY_FALLBACK.to_string()
    } else {
        String::new()
    }
}

/// Truncates a string to fit within a byte limit
pub fn truncate_to_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let note = "\n[truncated]";
    if max_bytes <= note.len() {
        // Not enough room for text + note: return as many whole chars as fit.
        return input.chars().take(max_bytes).collect();
    }

    let mut end = max_bytes - note.len();
    while !input.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    let mut truncated = input[..end].to_string();
    truncated.push_str(note);
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TextContent;

    #[test]
    fn render_prompt_for_display_prefers_user_text() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello".to_string())),
            ContentBlock::Text(TextContent::new("[file: x]".to_string())),
        ];

        assert_eq!(render_prompt_for_display(&blocks), "hello\n\n[file: x]");
        assert_eq!(format_prompt_user_text_only(&blocks), "hello\n\n[file: x]");
    }

    #[test]
    fn render_prompt_for_display_uses_attachment_placeholder() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("".to_string())),
            ContentBlock::Text(TextContent::new("[file: x]".to_string())),
        ];

        assert_eq!(render_prompt_for_display(&blocks), "[file: x]");
    }
}
