//! Message conversion utilities for llama.cpp provider.
//!
//! This module provides unified message handling for both tool-aware and basic chat paths.
//! It converts ChatMessages to either JSON format (for models with chat templates)
//! or simple text format (for raw prompt building).

use crate::config::LlamaCppConfig;
use querymt::chat::{
    ChatInputPart, ChatMessage, ChatMessagePart, ChatOutputItem, ChatRole, MediaKind, MediaSource,
    ToolResultPart,
};
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

        // For a structured assistant turn, text comes from its `Message` items so
        // reasoning/call items are not duplicated into the content string. Other
        // turns project their canonical input parts.
        let parts = match msg.output() {
            Some(output) => output
                .items
                .iter()
                .filter_map(|item| match item {
                    ChatOutputItem::Message(message) => Some(
                        message
                            .parts
                            .iter()
                            .filter_map(|part| match part {
                                ChatMessagePart::Text { text, .. } if !text.is_empty() => {
                                    Some(ChatInputPart::text(text.clone()))
                                }
                                ChatMessagePart::Refusal { refusal, .. } if !refusal.is_empty() => {
                                    Some(ChatInputPart::text(refusal.clone()))
                                }
                                ChatMessagePart::Media(media) => {
                                    Some(ChatInputPart::attachment((**media).clone()))
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>(),
            None => msg.portable_input_parts(),
        };

        // Visible reasoning is replayed from structured output.
        let thinking = msg
            .output()
            .and_then(|output| output.thinking())
            .unwrap_or_default();

        let mut tool_calls_array: Vec<Value> = Vec::new();
        // Count only inline images. This must exactly match what extract_media() collects.
        let mut image_count: usize = 0;

        // Generated function calls are replayed from structured output.
        if let Some(output) = msg.output() {
            for item in &output.items {
                if let ChatOutputItem::FunctionCall(call) = item {
                    let arguments = call.parse_arguments().unwrap_or(Value::Null);
                    tool_calls_array.push(serde_json::json!({
                        "id": call.call_id,
                        "type": "function",
                        "function": {
                            "name": call.name,
                            "arguments": arguments
                        }
                    }));
                }
            }
        }

        // Build text by interleaving markers at exact image positions.
        // This ensures N images produce exactly N markers in the text.
        let mut text_parts: Vec<String> = Vec::new();
        let mut pending_markers: usize = 0;

        for part in &parts {
            match part {
                ChatInputPart::Text { text } => {
                    // Flush any pending image markers before the next text segment.
                    for _ in 0..pending_markers {
                        text_parts.push(marker.to_string());
                    }
                    pending_markers = 0;
                    text_parts.push(text.clone());
                }
                ChatInputPart::Attachment(media) => match (&media.kind, media.source()) {
                    (MediaKind::Image, MediaSource::Inline { .. }) => {
                        image_count += 1;
                        pending_markers += 1;
                    }
                    (_, MediaSource::DataUrl { .. } | MediaSource::Url { .. }) => {
                        // Referenced media is not supported by extract_media — skip
                        // the marker to avoid a count mismatch.
                        log::warn!(
                            "Referenced media in message skipped (not supported for multimodal)"
                        );
                    }
                    _ => {}
                },
                ChatInputPart::ToolResult(result) => {
                    let output_text = result.text_content();
                    json_messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": result.call_id,
                        "name": result.name.clone().unwrap_or_default(),
                        "content": output_text
                    }));

                    // Count only inline image parts inside tool results
                    // (matching extract_media).
                    let nested_images = result
                        .parts
                        .iter()
                        .filter(|part| {
                            matches!(
                                part,
                                ToolResultPart::Attachment(media)
                                    if media.kind == MediaKind::Image
                                        && matches!(media.source(), MediaSource::Inline { .. })
                            )
                        })
                        .count();
                    for _ in 0..nested_images {
                        json_messages.push(serde_json::json!({
                            "role": "user",
                            "content": marker,
                        }));
                        media_count += 1;
                    }

                    // Warn about skipped referenced media in tool results.
                    let skipped_urls = result
                        .parts
                        .iter()
                        .filter(|part| {
                            matches!(
                                part,
                                ToolResultPart::Attachment(media)
                                    if matches!(
                                        media.source(),
                                        MediaSource::DataUrl { .. } | MediaSource::Url { .. }
                                    )
                            )
                        })
                        .count();
                    if skipped_urls > 0 {
                        log::warn!(
                            "Skipped {} referenced media part(s) inside ToolResult (not supported for multimodal)",
                            skipped_urls
                        );
                    }
                }
            }
        }

        // Flush any trailing image markers (images at the end of a message).
        for _ in 0..pending_markers {
            text_parts.push(marker.to_string());
        }

        media_count += image_count;

        if !tool_calls_array.is_empty() {
            // Some model templates apply string operations to assistant content.
            // Keep tool-only assistant messages compatible by emitting an empty string.
            let content = Value::String(text_parts.join("\n"));

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
    // Check for binary/attachment content - not supported in text-only mode.
    if messages
        .iter()
        .flat_map(ChatMessage::portable_input_parts)
        .any(|part| matches!(part, ChatInputPart::Attachment(_)))
    {
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
/// Tool results are rendered into text blocks.
fn normalize_messages_to_text(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|msg| {
            let mut builder = match msg.role {
                ChatRole::User => ChatMessage::user(),
                ChatRole::Assistant => ChatMessage::assistant(),
            };
            if let Some(cache) = msg.cache.clone() {
                builder = builder.cache(cache);
            }

            for part in msg.portable_input_parts() {
                match part {
                    ChatInputPart::Text { text } => {
                        builder = builder.part(ChatInputPart::text(text));
                    }
                    ChatInputPart::ToolResult(result) => {
                        builder = builder.part(ChatInputPart::text(format!(
                            "[ToolResult: {}] {}",
                            result.call_id,
                            result.text_content().replace('\n', "\\n")
                        )));
                    }
                    ChatInputPart::Attachment(_) => {
                        // Unreachable: binary content is rejected before this point.
                    }
                }
            }

            // Assistant reasoning/function-call output is flattened to text for
            // template-less prompt building.
            if let Some(output) = msg.output() {
                for item in &output.items {
                    match item {
                        ChatOutputItem::FunctionCall(call) => {
                            builder = builder.part(ChatInputPart::text(format!(
                                "[ToolUse: {} ({}) args={}]",
                                call.name, call.call_id, call.arguments
                            )));
                        }
                        _ => {}
                    }
                }
            }

            builder.build()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{
        ChatInputPart, ChatOutputItem, MediaKind, MediaPart, MediaSource, ToolResult,
        ToolResultPart,
    };

    /// Test-local content block.
    ///
    /// The public legacy `Content` enum no longer exists, so fixtures express
    /// their intent in this small enum and convert explicitly. Generated
    /// semantics (`thinking`, `tool_use`) are kept distinct from supplied input
    /// because only the former belongs to `ChatOutput`.
    enum Block {
        Text(String),
        Thinking(String),
        ToolUse {
            id: String,
            name: String,
            arguments: serde_json::Value,
        },
        Image(String, Vec<u8>),
        ImageUrl(String),
        ToolResult {
            id: String,
            name: Option<String>,
            is_error: bool,
            content: Vec<Block>,
        },
    }

    fn text(s: &str) -> Block {
        Block::Text(s.to_string())
    }

    fn image(mime: &str, data: Vec<u8>) -> Block {
        Block::Image(mime.to_string(), data)
    }

    fn image_url(url: &str) -> Block {
        Block::ImageUrl(url.to_string())
    }

    fn thinking(s: &str) -> Block {
        Block::Thinking(s.to_string())
    }

    fn tool_use(id: &str, name: &str, arguments: serde_json::Value) -> Block {
        Block::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }
    }

    fn tool_result(id: impl Into<String>, content: Vec<Block>) -> Block {
        Block::ToolResult {
            id: id.into(),
            name: None,
            is_error: false,
            content,
        }
    }

    /// Tool result carrying a tool name, which providers surface as `name`.
    fn named_tool_result(id: impl Into<String>, name: &str, content: Vec<Block>) -> Block {
        Block::ToolResult {
            id: id.into(),
            name: Some(name.to_string()),
            is_error: false,
            content,
        }
    }

    /// Build a canonical inline attachment input part.
    fn attachment(kind: MediaKind, mime: &str, data: Vec<u8>) -> ChatInputPart {
        ChatInputPart::attachment(
            MediaPart::new(
                kind,
                Some(mime.parse().expect("valid media type")),
                MediaSource::Inline { data },
            )
            .expect("valid inline attachment"),
        )
    }

    fn test_config() -> LlamaCppConfig {
        LlamaCppConfig {
            model: "test.gguf".to_string(),
            max_tokens: None,
            temperature: None,
            top_p: None,
            min_p: None,
            top_k: None,
            repeat_penalty: None,
            presence_penalty: None,
            frequency_penalty: None,
            penalty_last_n: None,
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
            enable_thinking: None,
            reasoning_effort: None,
            preserve_reasoning: None,
            flash_attention: None,
            kv_cache_type_k: None,
            kv_cache_type_v: None,
            mmproj_path: None,
            media_marker: None,
            mmproj_threads: None,
            mmproj_use_gpu: None,
            n_ubatch: None,
            text_only: None,
            speculative: None,
            backend_sampling: None,
            json_schema: None,
        }
    }

    fn user_msg(blocks: Vec<Block>) -> ChatMessage {
        let mut builder = ChatMessage::user();
        for part in user_input_parts(blocks) {
            builder = builder.part(part);
        }
        builder.build()
    }

    /// Convert test blocks into canonical input parts for a user turn.
    ///
    /// Generated semantics (thinking, tool use) are dropped: they have no
    /// ordinary-input representation.
    fn user_input_parts(blocks: Vec<Block>) -> Vec<ChatInputPart> {
        blocks.into_iter().filter_map(user_input_part).collect()
    }

    fn user_input_part(block: Block) -> Option<ChatInputPart> {
        match block {
            Block::Text(text) => Some(ChatInputPart::text(text)),
            Block::Image(mime, data) => {
                let kind = if mime.starts_with("image/") {
                    MediaKind::Image
                } else if mime == "application/pdf" {
                    MediaKind::Document
                } else {
                    MediaKind::Audio
                };
                Some(attachment(kind, &mime, data))
            }
            Block::ToolResult {
                id,
                name,
                is_error,
                content,
            } => {
                let mut result = ToolResult::new(id);
                result.name = name;
                result.is_error = is_error;
                result.parts = content
                    .into_iter()
                    .filter_map(|inner| match inner {
                        Block::Text(text) => Some(ToolResultPart::text(text)),
                        Block::Image(mime, data) => Some(ToolResultPart::Attachment(Box::new(
                            MediaPart::new(
                                if mime.starts_with("image/") {
                                    MediaKind::Image
                                } else {
                                    MediaKind::Document
                                },
                                Some(mime.parse().expect("valid media type")),
                                MediaSource::Inline { data },
                            )
                            .expect("valid inline attachment"),
                        ))),
                        _ => None,
                    })
                    .collect();
                Some(ChatInputPart::tool_result(result))
            }
            Block::ImageUrl(_) | Block::Thinking(_) | Block::ToolUse { .. } => None,
        }
    }

    fn assistant_msg(blocks: Vec<Block>) -> ChatMessage {
        // Mixed assistant turns become structured output; reasoning and calls
        // are generated semantics and only live in `ChatOutput`.
        let mut items = Vec::new();
        let mut text_parts: Vec<querymt::chat::ChatMessagePart> = Vec::new();
        for block in blocks {
            match block {
                Block::Thinking(text) => {
                    if !text_parts.is_empty() {
                        items.push(ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                            id: None,
                            role: ChatRole::Assistant,
                            phase: None,
                            status: None,
                            parts: std::mem::take(&mut text_parts),
                            extensions: Default::default(),
                        }));
                    }
                    items.push(ChatOutputItem::Reasoning(
                        querymt::chat::ChatReasoningItem {
                            id: None,
                            summary: vec![querymt::chat::ChatReasoningPart::text(text)],
                            content: Vec::new(),
                            encrypted_content: None,
                            signature: None,
                            status: None,
                            extensions: Default::default(),
                        },
                    ));
                }
                Block::ToolUse {
                    id,
                    name,
                    arguments,
                } => {
                    if !text_parts.is_empty() {
                        items.push(ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                            id: None,
                            role: ChatRole::Assistant,
                            phase: None,
                            status: None,
                            parts: std::mem::take(&mut text_parts),
                            extensions: Default::default(),
                        }));
                    }
                    items.push(ChatOutputItem::FunctionCall(
                        querymt::chat::ChatFunctionCallItem {
                            item_id: None,
                            call_id: id,
                            name,
                            arguments: serde_json::to_string(&arguments).unwrap_or_default(),
                            status: None,
                            extensions: Default::default(),
                        },
                    ));
                }
                Block::Text(text) => {
                    text_parts.push(querymt::chat::ChatMessagePart::Text {
                        text,
                        annotations: Vec::new(),
                        extensions: Default::default(),
                    });
                }
                _ => {}
            }
        }
        if !text_parts.is_empty() {
            items.push(ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                id: None,
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: text_parts,
                extensions: Default::default(),
            }));
        }
        ChatMessage::from(querymt::chat::ChatOutput {
            items,
            ..querymt::chat::ChatOutput::default()
        })
    }

    #[test]
    fn basic_text_messages() {
        let cfg = test_config();
        let messages = vec![
            user_msg(vec![text("Hello")]),
            assistant_msg(vec![text("Hi there!")]),
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

        let messages = vec![user_msg(vec![text("Hello")])];

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
            thinking("Let me calculate..."),
            text("The answer is 42"),
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
            text("Let me check"),
            tool_use(
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
        assert_eq!(
            parsed[0]["tool_calls"][0]["function"]["arguments"],
            serde_json::json!({"city": "Paris"})
        );
        assert!(parsed[0]["tool_calls"][0]["function"]["arguments"].is_object());
    }

    #[test]
    fn tool_use_without_text_uses_empty_string_content() {
        let cfg = test_config();
        let messages = vec![assistant_msg(vec![
            thinking("Let me inspect the directory"),
            tool_use(
                "call_123",
                "ls",
                serde_json::json!({"path": "crates/provider-common/src"}),
            ),
        ])];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "assistant");
        assert_eq!(parsed[0]["content"], "");
        assert_eq!(
            parsed[0]["reasoning_content"],
            "Let me inspect the directory"
        );
        assert_eq!(parsed[0]["tool_calls"][0]["id"], "call_123");
    }

    #[test]
    fn multi_turn_tool_history_has_string_assistant_content() {
        let cfg = test_config();
        let messages = vec![
            user_msg(vec![text("Analyze provider-common")]),
            assistant_msg(vec![
                thinking("Start with the manifest"),
                text("I'll inspect the manifest."),
                tool_use(
                    "call_manifest",
                    "read_tool",
                    serde_json::json!({"path": "crates/provider-common/Cargo.toml"}),
                ),
            ]),
            user_msg(vec![named_tool_result(
                "call_manifest".to_string(),
                "read_tool",
                vec![text("[package]\nname = provider-common")],
            )]),
            assistant_msg(vec![
                thinking("Now inspect the sources"),
                tool_use(
                    "call_sources",
                    "ls",
                    serde_json::json!({"path": "crates/provider-common/src"}),
                ),
            ]),
            user_msg(vec![named_tool_result(
                "call_sources".to_string(),
                "ls",
                vec![text("lib.rs")],
            )]),
        ];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 5);
        assert_eq!(parsed[1]["content"], "I'll inspect the manifest.");
        assert_eq!(parsed[2]["role"], "tool");
        assert_eq!(parsed[3]["role"], "assistant");
        assert_eq!(parsed[3]["content"], "");
        assert!(parsed[3]["content"].is_string());
        assert_eq!(parsed[3]["tool_calls"][0]["id"], "call_sources");
        assert_eq!(parsed[4]["tool_call_id"], "call_sources");
    }

    #[test]
    fn tool_result_message() {
        let cfg = test_config();
        let messages = vec![user_msg(vec![named_tool_result(
            "call_123".to_string(),
            "get_weather",
            vec![text(r#"{"temperature": 22}"#)],
        )])];

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
            image("image/jpeg", vec![0xFF, 0xD8, 0xFF]),
            text("What's in this image?"),
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
        let messages = vec![user_msg(vec![image(
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
            image("image/png", vec![1]),
            image("image/png", vec![2]),
            image("image/png", vec![3]),
            text("Describe all three images"),
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
            image("image/png", vec![1]),
            text("First image above."),
            image("image/png", vec![2]),
            text("Second image above."),
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
        let messages = vec![user_msg(vec![tool_result(
            "call_1".to_string(),
            vec![
                text("Photo metadata here"),
                image("image/png", vec![0x89, 0x50]),
            ],
        )])];

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
        let messages = vec![user_msg(vec![named_tool_result(
            "call_1".to_string(),
            "photos_search",
            vec![
                text("metadata"),
                image("image/png", vec![1]),
                image("image/png", vec![2]),
                image("image/jpeg", vec![3]),
            ],
        )])];

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
            Block::ImageUrl("https://example.com/photo.jpg".to_string()),
            text("What is this?"),
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
        let messages = vec![user_msg(vec![named_tool_result(
            "call_1".to_string(),
            "tool",
            vec![text("result"), image_url("https://example.com/img.png")],
        )])];

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
            user_msg(vec![text("Hello")]),
            assistant_msg(vec![text("Hi there!")]),
        ];

        let result = messages_to_text(&cfg, &messages).unwrap();
        assert_eq!(result, "Hello\n\nHi there!");
    }

    #[test]
    fn text_with_system_prompt() {
        let mut cfg = test_config();
        cfg.system = vec!["You are helpful".to_string()];

        let messages = vec![user_msg(vec![text("Hello")])];

        let result = messages_to_text(&cfg, &messages).unwrap();
        assert_eq!(result, "You are helpful\n\nHello");
    }

    #[test]
    fn text_normalizes_tool_messages() {
        let cfg = test_config();
        let messages = vec![
            user_msg(vec![text("Search for rust")]),
            assistant_msg(vec![
                text("Searching..."),
                tool_use("call_123", "search", serde_json::json!({"query": "rust"})),
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
            text("Look at this"),
            image("image/png", vec![1, 2, 3]),
        ])];

        let result = messages_to_text(&cfg, &messages);
        assert!(result.is_err());
    }
}
