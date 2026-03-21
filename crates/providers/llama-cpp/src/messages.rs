//! Message conversion utilities for llama.cpp provider.
//!
//! This module provides unified message handling for both tool-aware and basic chat paths.
//! It converts ChatMessages to either JSON format (for models with chat templates)
//! or simple text format (for raw prompt building).

use crate::config::LlamaCppConfig;
use querymt::chat::{ChatMessage, ChatRole, Content};
use querymt::error::LLMError;
use serde_json::Value;

/// Convert ChatMessages to JSON array for template application.
/// This is the unified path for both tool-aware and basic conversations.
///
/// Now handles images by injecting media markers and extracting media data.
///
/// # Arguments
/// * `cfg` - The llama.cpp configuration containing system prompts
/// * `messages` - The chat messages to convert
/// * `media_marker` - Optional media marker string for image positions
///
/// # Returns
/// A tuple of (json_string, media_count) where media_count tells the caller
/// how many bitmaps to prepare in order.
pub(crate) fn messages_to_json(
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
    media_marker: Option<&str>,
) -> Result<(String, usize), LLMError> {
    let mut json_messages = Vec::new();
    let mut media_count = 0;
    let marker = media_marker.unwrap_or("");

    // Add system message if configured
    if !cfg.system.is_empty() {
        let system = cfg.system.join("\n\n");
        json_messages.push(serde_json::json!({
            "role": "system",
            "content": system
        }));
    }

    for msg in messages {
        let role = match msg.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        };

        let thinking = msg
            .content
            .iter()
            .filter_map(|b| match b {
                Content::Thinking { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let mut tool_calls_array: Vec<Value> = Vec::new();
        // Count only Content::Image (not ImageUrl — unsupported for now).
        // This must exactly match what extract_media() collects.
        let mut image_count: usize = 0;

        // Build text by interleaving markers at exact image positions.
        // This ensures N images produce exactly N markers in the text.
        let mut text_parts: Vec<String> = Vec::new();
        let mut pending_markers: usize = 0;

        for block in &msg.content {
            match block {
                Content::Text { text } => {
                    // Flush any pending image markers before the next text segment.
                    for _ in 0..pending_markers {
                        text_parts.push(marker.to_string());
                    }
                    pending_markers = 0;
                    text_parts.push(text.clone());
                }
                Content::Image { .. } => {
                    image_count += 1;
                    pending_markers += 1;
                }
                Content::ImageUrl { .. } => {
                    // ImageUrl is not supported by extract_media — skip marker
                    // to avoid count mismatch. Log a warning.
                    log::warn!("ImageUrl in message skipped (not supported for multimodal)");
                }
                Content::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    tool_calls_array.push(serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(arguments).unwrap_or_default()
                        }
                    }));
                }
                Content::ToolResult {
                    id, name, content, ..
                } => {
                    let output_text = content
                        .iter()
                        .filter_map(|c| c.as_text())
                        .collect::<Vec<_>>()
                        .join("\n");
                    json_messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": id,
                        "name": name.clone().unwrap_or_default(),
                        "content": output_text
                    }));

                    // Count only Content::Image inside tool results (matching extract_media).
                    let nested_images = content
                        .iter()
                        .filter(|c| matches!(c, Content::Image { .. }))
                        .count();
                    for _ in 0..nested_images {
                        json_messages.push(serde_json::json!({
                            "role": "user",
                            "content": marker,
                        }));
                        media_count += 1;
                    }

                    // Warn about skipped ImageUrl in tool results.
                    let skipped_urls = content
                        .iter()
                        .filter(|c| matches!(c, Content::ImageUrl { .. }))
                        .count();
                    if skipped_urls > 0 {
                        log::warn!(
                            "Skipped {} ImageUrl block(s) inside ToolResult (not supported for multimodal)",
                            skipped_urls
                        );
                    }
                }
                _ => {}
            }
        }

        // Flush any trailing image markers (images at the end of a message).
        for _ in 0..pending_markers {
            text_parts.push(marker.to_string());
        }

        media_count += image_count;

        if !tool_calls_array.is_empty() {
            let text = text_parts.join("\n");
            let content = if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            };

            let mut json_msg = serde_json::json!({
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls_array
            });
            if !thinking.is_empty() {
                json_msg["reasoning_content"] = serde_json::json!(thinking);
            }
            json_messages.push(json_msg);
            continue;
        }

        let text = text_parts.join("\n");

        if !text.is_empty() || !thinking.is_empty() {
            let mut json_msg = serde_json::json!({
                "role": role,
                "content": text
            });
            if !thinking.is_empty() {
                json_msg["reasoning_content"] = serde_json::json!(thinking);
            }
            json_messages.push(json_msg);
        }
    }

    let json = serde_json::to_string(&json_messages).map_err(|e| {
        LLMError::ProviderError(format!("Failed to serialize messages JSON: {}", e))
    })?;

    Ok((json, media_count))
}

/// Convert ChatMessages to simple text prompt (fallback for models without templates).
/// This normalizes ToolUse/ToolResult to Text and concatenates all messages.
///
/// # Arguments
/// * `cfg` - The llama.cpp configuration containing system prompts
/// * `messages` - The chat messages to convert
///
/// # Returns
/// A simple text string with all messages concatenated
pub(crate) fn messages_to_text(
    cfg: &LlamaCppConfig,
    messages: &[ChatMessage],
) -> Result<String, LLMError> {
    // Check for binary/image content - not supported in text-only mode.
    if messages.iter().flat_map(|m| m.content.iter()).any(|b| {
        matches!(
            b,
            Content::Image { .. }
                | Content::ImageUrl { .. }
                | Content::Pdf { .. }
                | Content::Audio { .. }
        )
    }) {
        return Err(LLMError::InvalidRequest(
            "Binary content not supported in text-only mode (model lacks chat template or multimodal support)".into(),
        ));
    }

    // Normalize tool messages to text for basic prompt building.
    let normalized = normalize_messages_to_text(messages);

    let mut prompt = String::new();
    if !cfg.system.is_empty() {
        prompt.push_str(&cfg.system.join("\n\n"));
        prompt.push_str("\n\n");
    }
    for (idx, msg) in normalized.iter().enumerate() {
        prompt.push_str(&msg.text());
        if idx + 1 < normalized.len() {
            prompt.push_str("\n\n");
        }
    }
    Ok(prompt)
}

/// Normalize messages for providers that don't support structured tool messages.
/// ToolUse/ToolResult blocks are rendered into text blocks.
fn normalize_messages_to_text(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|msg| {
            let mut out_blocks = Vec::new();
            for block in &msg.content {
                match block {
                    Content::Text { .. } | Content::Thinking { .. } => {
                        out_blocks.push(block.clone())
                    }
                    Content::ToolUse {
                        id,
                        name,
                        arguments,
                    } => out_blocks.push(Content::text(format!(
                        "[ToolUse: {name} ({id}) args={}]",
                        serde_json::to_string(arguments).unwrap_or_default()
                    ))),
                    Content::ToolResult { id, content, .. } => {
                        out_blocks.push(Content::text(format!(
                            "[ToolResult: {id}] {}",
                            content
                                .iter()
                                .filter_map(|c| c.as_text())
                                .collect::<Vec<_>>()
                                .join("\\n")
                        )))
                    }
                    _ => out_blocks.push(block.clone()),
                }
            }

            ChatMessage {
                role: msg.role.clone(),
                content: out_blocks,
                cache: msg.cache.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> LlamaCppConfig {
        LlamaCppConfig {
            model: "test.gguf".to_string(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            min_p: None,
            top_k: None,
            system: vec![],
            n_ctx: None,
            n_batch: None,
            n_threads: None,
            n_threads_batch: None,
            n_gpu_layers: None,
            seed: None,
            chat_template: None,
            use_chat_template: None,
            add_bos: None,
            log: None,
            fast_download: None,
            enable_thinking: None,
            flash_attention: None,
            kv_cache_type_k: None,
            kv_cache_type_v: None,
            mmproj_path: None,
            media_marker: None,
            mmproj_threads: None,
            mmproj_use_gpu: None,
            n_ubatch: None,
            text_only: None,
            json_schema: None,
        }
    }

    fn user_msg(blocks: Vec<Content>) -> ChatMessage {
        ChatMessage {
            role: ChatRole::User,
            content: blocks,
            cache: None,
        }
    }

    fn assistant_msg(blocks: Vec<Content>) -> ChatMessage {
        ChatMessage {
            role: ChatRole::Assistant,
            content: blocks,
            cache: None,
        }
    }

    #[test]
    fn basic_text_messages() {
        let cfg = test_config();
        let messages = vec![
            user_msg(vec![Content::text("Hello")]),
            assistant_msg(vec![Content::text("Hi there!")]),
        ];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "Hello");
        assert_eq!(parsed[1]["role"], "assistant");
        assert_eq!(parsed[1]["content"], "Hi there!");
    }

    #[test]
    fn system_message_prepended() {
        let mut cfg = test_config();
        cfg.system = vec!["You are a helpful assistant".to_string()];

        let messages = vec![user_msg(vec![Content::text("Hello")])];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "system");
        assert_eq!(parsed[0]["content"], "You are a helpful assistant");
        assert_eq!(parsed[1]["role"], "user");
    }

    #[test]
    fn thinking_block_emitted() {
        let cfg = test_config();
        let messages = vec![assistant_msg(vec![
            Content::thinking("Let me calculate..."),
            Content::text("The answer is 42"),
        ])];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "assistant");
        assert_eq!(parsed[0]["content"], "The answer is 42");
        assert_eq!(parsed[0]["reasoning_content"], "Let me calculate...");
    }

    #[test]
    fn tool_use_message() {
        let cfg = test_config();
        let messages = vec![assistant_msg(vec![
            Content::text("Let me check"),
            Content::tool_use(
                "call_123",
                "get_weather",
                serde_json::json!({"city": "Paris"}),
            ),
        ])];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "assistant");
        assert_eq!(parsed[0]["content"], "Let me check");
        assert!(parsed[0]["tool_calls"].is_array());
        assert_eq!(parsed[0]["tool_calls"][0]["id"], "call_123");
        assert_eq!(
            parsed[0]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
    }

    #[test]
    fn tool_result_message() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![Content::ToolResult {
            id: "call_123".to_string(),
            name: Some("get_weather".to_string()),
            is_error: false,
            content: vec![Content::text(r#"{"temperature": 22}"#)],
        }])];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "tool");
        assert_eq!(parsed[0]["tool_call_id"], "call_123");
        assert_eq!(parsed[0]["name"], "get_weather");
        assert_eq!(parsed[0]["content"], r#"{"temperature": 22}"#);
    }

    #[test]
    fn single_image_with_text() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![
            Content::image("image/jpeg", vec![0xFF, 0xD8, 0xFF]),
            Content::text("What's in this image?"),
        ])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<image>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 1);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "<image>\nWhat's in this image?");
    }

    #[test]
    fn single_image_no_text() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![Content::image(
            "image/png",
            vec![0x89, 0x50, 0x4E, 0x47],
        )])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<__media__>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 1);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "<__media__>");
    }

    #[test]
    fn multiple_images_produce_multiple_markers() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![
            Content::image("image/png", vec![1]),
            Content::image("image/png", vec![2]),
            Content::image("image/png", vec![3]),
            Content::text("Describe all three images"),
        ])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<M>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 3);
        assert_eq!(parsed.len(), 1);
        let content = parsed[0]["content"].as_str().unwrap();
        assert_eq!(
            content.matches("<M>").count(),
            3,
            "Expected 3 markers, got: {}",
            content
        );
    }

    #[test]
    fn images_interleaved_with_text() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![
            Content::image("image/png", vec![1]),
            Content::text("First image above."),
            Content::image("image/png", vec![2]),
            Content::text("Second image above."),
        ])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<M>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 2);
        let content = parsed[0]["content"].as_str().unwrap();
        assert_eq!(content.matches("<M>").count(), 2);
        // Markers should appear before their respective text segments.
        assert!(content.contains("<M>\nFirst image above."));
        assert!(content.contains("<M>\nSecond image above."));
    }

    #[test]
    fn tool_result_with_nested_image() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![Content::ToolResult {
            id: "call_1".to_string(),
            name: Some("photos_recent".to_string()),
            is_error: false,
            content: vec![
                Content::text("Photo metadata here"),
                Content::image("image/png", vec![0x89, 0x50]),
            ],
        }])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<__media__>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 1);
        // Should produce: tool message + separate user message with marker
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "tool");
        assert_eq!(parsed[0]["content"], "Photo metadata here");
        assert_eq!(parsed[1]["role"], "user");
        assert_eq!(parsed[1]["content"], "<__media__>");
    }

    #[test]
    fn tool_result_with_multiple_nested_images() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![Content::ToolResult {
            id: "call_1".to_string(),
            name: Some("photos_search".to_string()),
            is_error: false,
            content: vec![
                Content::text("metadata"),
                Content::image("image/png", vec![1]),
                Content::image("image/png", vec![2]),
                Content::image("image/jpeg", vec![3]),
            ],
        }])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<M>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 3);
        // 1 tool message + 3 user marker messages
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0]["role"], "tool");
        for i in 1..=3 {
            assert_eq!(parsed[i]["role"], "user");
            assert_eq!(parsed[i]["content"], "<M>");
        }
    }

    #[test]
    fn image_url_skipped_no_marker() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![
            Content::image_url("https://example.com/photo.jpg"),
            Content::text("What is this?"),
        ])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<M>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        // ImageUrl is unsupported — no marker, no media count
        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["content"], "What is this?");
    }

    #[test]
    fn image_url_in_tool_result_skipped() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![Content::ToolResult {
            id: "call_1".to_string(),
            name: Some("tool".to_string()),
            is_error: false,
            content: vec![
                Content::text("result"),
                Content::image_url("https://example.com/img.png"),
            ],
        }])];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<M>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        // ImageUrl inside ToolResult is unsupported — no marker
        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "tool");
    }

    #[test]
    fn text_only_messages_to_text() {
        let cfg = test_config();
        let messages = vec![
            user_msg(vec![Content::text("Hello")]),
            assistant_msg(vec![Content::text("Hi there!")]),
        ];

        let result = messages_to_text(&cfg, &messages).unwrap();
        assert_eq!(result, "Hello\n\nHi there!");
    }

    #[test]
    fn text_with_system_prompt() {
        let mut cfg = test_config();
        cfg.system = vec!["You are helpful".to_string()];

        let messages = vec![user_msg(vec![Content::text("Hello")])];

        let result = messages_to_text(&cfg, &messages).unwrap();
        assert_eq!(result, "You are helpful\n\nHello");
    }

    #[test]
    fn text_normalizes_tool_messages() {
        let cfg = test_config();
        let messages = vec![
            user_msg(vec![Content::text("Search for rust")]),
            assistant_msg(vec![
                Content::text("Searching..."),
                Content::tool_use("call_123", "search", serde_json::json!({"query": "rust"})),
            ]),
        ];

        let result = messages_to_text(&cfg, &messages).unwrap();
        // Tool use is normalized — text content is preserved, tool block becomes text
        assert!(result.contains("Search for rust"));
        assert!(result.contains("Searching..."));
        assert!(result.contains("[ToolUse: search"));
    }

    #[test]
    fn text_mode_rejects_binary_content() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![
            Content::text("Look at this"),
            Content::image("image/png", vec![1, 2, 3]),
        ])];

        let result = messages_to_text(&cfg, &messages);
        assert!(result.is_err());
    }
}
