//! Message conversion utilities for llama.cpp provider.
//!
//! This module provides unified message handling for both tool-aware and basic chat paths.
//! It converts ChatMessages to either JSON format (for models with chat templates)
//! or simple text format (for raw prompt building).

use crate::config::LlamaCppConfig;
use querymt::chat::{ChatMessage, ChatRole, MessageType};
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
        match &msg.message_type {
            MessageType::Text => {
                let role = match msg.role {
                    ChatRole::User => "user",
                    ChatRole::Assistant => "assistant",
                };

                // For assistant messages, separate <think> blocks from content
                // into reasoning_content for the template engine.
                // If thinking was already extracted (msg.thinking is Some), use it.
                // Otherwise, extract from content as a fallback for messages
                // stored before thinking extraction was available.
                let (thinking, content) = if msg.thinking.is_some() {
                    (msg.thinking.clone(), msg.content.clone())
                } else if matches!(msg.role, ChatRole::Assistant) {
                    let (t, c) = querymt::chat::extract_thinking(&msg.content);
                    (t, c)
                } else {
                    (None, msg.content.clone())
                };

                let mut json_msg = serde_json::json!({
                    "role": role,
                    "content": content
                });
                if let Some(ref t) = thinking {
                    if !t.is_empty() {
                        json_msg["reasoning_content"] = serde_json::json!(t);
                    }
                }
                json_messages.push(json_msg);
            }
            MessageType::ToolUse(tool_calls) => {
                // Assistant message with tool calls in OpenAI format
                let tool_calls_array: Vec<Value> = tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": tc.call_type,
                            "function": {
                                "name": tc.function.name,
                                "arguments": tc.function.arguments
                            }
                        })
                    })
                    .collect();

                // Separate <think> blocks from content (fallback extraction)
                let (thinking, clean_content) = if msg.thinking.is_some() {
                    (msg.thinking.clone(), msg.content.clone())
                } else {
                    let (t, c) = querymt::chat::extract_thinking(&msg.content);
                    (t, c)
                };

                let content = if clean_content.is_empty() {
                    Value::Null
                } else {
                    Value::String(clean_content)
                };

                let mut json_msg = serde_json::json!({
                    "role": "assistant",
                    "content": content,
                    "tool_calls": tool_calls_array
                });
                if let Some(ref t) = thinking {
                    if !t.is_empty() {
                        json_msg["reasoning_content"] = serde_json::json!(t);
                    }
                }
                json_messages.push(json_msg);
            }
            MessageType::ToolResult(results) => {
                // Tool results - each result is a separate message with tool role
                // Note: function.arguments contains the result content,
                // function.name contains the tool name, and id is the tool_call_id
                for result in results {
                    json_messages.push(serde_json::json!({
                        "role": "tool",
                        "tool_call_id": result.id,
                        "name": result.function.name,
                        "content": result.function.arguments
                    }));
                }
            }
            MessageType::Image(_) => {
                // Inject marker into content
                let role = match msg.role {
                    ChatRole::User => "user",
                    ChatRole::Assistant => "assistant",
                };

                let content = if msg.content.is_empty() {
                    // If no text, just the marker
                    marker.to_string()
                } else {
                    // Prepend marker before text
                    format!("{}\n{}", marker, msg.content)
                };

                json_messages.push(serde_json::json!({
                    "role": role,
                    "content": content
                }));

                media_count += 1;
            }
            MessageType::ImageURL(_) => {
                // Same as Image for now (media extraction handles the difference)
                let role = match msg.role {
                    ChatRole::User => "user",
                    ChatRole::Assistant => "assistant",
                };

                let content = if msg.content.is_empty() {
                    marker.to_string()
                } else {
                    format!("{}\n{}", marker, msg.content)
                };

                json_messages.push(serde_json::json!({
                    "role": role,
                    "content": content
                }));

                media_count += 1;
            }
            _ => {
                return Err(LLMError::InvalidRequest(format!(
                    "MessageType {:?} not yet supported",
                    msg.message_type
                )));
            }
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
    // Check for images - not supported in text-only mode
    if messages.iter().any(|m| {
        matches!(
            m.message_type,
            MessageType::Image(_) | MessageType::ImageURL(_)
        )
    }) {
        return Err(LLMError::InvalidRequest(
            "Images not supported in text-only mode (model lacks chat template or multimodal support)".into(),
        ));
    }

    // Normalize tool messages to text for basic prompt building
    let normalized = normalize_messages_to_text(messages);

    // Validate that only text messages remain after normalization
    for msg in &normalized {
        if !matches!(msg.message_type, MessageType::Text) {
            return Err(LLMError::InvalidRequest(format!(
                "MessageType {:?} not supported in text-only mode",
                msg.message_type
            )));
        }
    }

    let mut prompt = String::new();
    if !cfg.system.is_empty() {
        prompt.push_str(&cfg.system.join("\n\n"));
        prompt.push_str("\n\n");
    }
    for (idx, msg) in normalized.iter().enumerate() {
        prompt.push_str(&msg.content);
        if idx + 1 < normalized.len() {
            prompt.push_str("\n\n");
        }
    }
    Ok(prompt)
}

/// Normalize messages to Text type for providers that don't support structured tool messages.
/// ToolUse and ToolResult messages are converted to Text, preserving their content.
/// Image/PDF/ImageURL messages are NOT normalized and will still error (appropriate behavior).
fn normalize_messages_to_text(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|msg| {
            match &msg.message_type {
                MessageType::Text => msg.clone(),
                MessageType::ToolUse(_) | MessageType::ToolResult(_) => {
                    // Tool messages already have text content populated by to_chat_message()
                    // We convert them to Text type to allow basic prompt building
                    ChatMessage {
                        role: msg.role.clone(),
                        message_type: MessageType::Text,
                        content: msg.content.clone(),
                        thinking: msg.thinking.clone(),
                        cache: msg.cache.clone(),
                    }
                }
                // Image/PDF/ImageURL will NOT be normalized - they should still error
                _ => msg.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::ImageMime;
    use querymt::{FunctionCall, ToolCall};

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
        }
    }

    #[test]
    fn test_messages_to_json_basic() {
        let cfg = test_config();
        let messages = vec![
            ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Text,
                content: "Hello".to_string(),
                thinking: None,
                cache: None,
            },
            ChatMessage {
                role: ChatRole::Assistant,
                message_type: MessageType::Text,
                content: "Hi there!".to_string(),
                thinking: None,
                cache: None,
            },
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
    fn test_messages_to_json_with_system() {
        let mut cfg = test_config();
        cfg.system = vec!["You are a helpful assistant".to_string()];

        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Text,
            content: "Hello".to_string(),
            thinking: None,
            cache: None,
        }];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["role"], "system");
        assert_eq!(parsed[0]["content"], "You are a helpful assistant");
        assert_eq!(parsed[1]["role"], "user");
    }

    #[test]
    fn test_messages_to_json_with_thinking() {
        let cfg = test_config();
        let messages = vec![ChatMessage {
            role: ChatRole::Assistant,
            message_type: MessageType::Text,
            content: "The answer is 42".to_string(),
            thinking: Some("Let me calculate this...".to_string()),
            cache: None,
        }];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "assistant");
        assert_eq!(parsed[0]["content"], "The answer is 42");
        assert_eq!(parsed[0]["reasoning_content"], "Let me calculate this...");
    }

    #[test]
    fn test_messages_to_json_with_tool_use() {
        let cfg = test_config();
        let tool_calls = vec![ToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city": "Paris"}"#.to_string(),
            },
        }];

        let messages = vec![ChatMessage {
            role: ChatRole::Assistant,
            message_type: MessageType::ToolUse(tool_calls),
            content: "Let me check the weather".to_string(),
            thinking: None,
            cache: None,
        }];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "assistant");
        assert_eq!(parsed[0]["content"], "Let me check the weather");
        assert!(parsed[0]["tool_calls"].is_array());
        assert_eq!(parsed[0]["tool_calls"][0]["id"], "call_123");
        assert_eq!(
            parsed[0]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
    }

    #[test]
    fn test_messages_to_json_with_tool_result() {
        let cfg = test_config();
        let tool_results = vec![ToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"temperature": 22, "condition": "sunny"}"#.to_string(),
            },
        }];

        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::ToolResult(tool_results),
            content: "Weather data".to_string(),
            thinking: None,
            cache: None,
        }];

        let (result, media_count) = messages_to_json(&cfg, &messages, None).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 0);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "tool");
        assert_eq!(parsed[0]["tool_call_id"], "call_123");
        assert_eq!(parsed[0]["name"], "get_weather");
        assert_eq!(
            parsed[0]["content"],
            r#"{"temperature": 22, "condition": "sunny"}"#
        );
    }

    #[test]
    fn test_messages_to_json_with_image() {
        let cfg = test_config();
        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Image((ImageMime::JPEG, vec![0xFF, 0xD8, 0xFF])),
            content: "What's in this image?".to_string(),
            thinking: None,
            cache: None,
        }];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<image>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 1);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "<image>\nWhat's in this image?");
    }

    #[test]
    fn test_messages_to_json_with_image_no_text() {
        let cfg = test_config();
        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Image((ImageMime::PNG, vec![0x89, 0x50, 0x4E, 0x47])),
            content: "".to_string(),
            thinking: None,
            cache: None,
        }];

        let (result, media_count) = messages_to_json(&cfg, &messages, Some("<__media__>")).unwrap();
        let parsed: Vec<Value> = serde_json::from_str(&result).unwrap();

        assert_eq!(media_count, 1);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "<__media__>");
    }

    #[test]
    fn test_messages_to_text_basic() {
        let cfg = test_config();
        let messages = vec![
            ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Text,
                content: "Hello".to_string(),
                thinking: None,
                cache: None,
            },
            ChatMessage {
                role: ChatRole::Assistant,
                message_type: MessageType::Text,
                content: "Hi there!".to_string(),
                thinking: None,
                cache: None,
            },
        ];

        let result = messages_to_text(&cfg, &messages).unwrap();
        assert_eq!(result, "Hello\n\nHi there!");
    }

    #[test]
    fn test_messages_to_text_with_system() {
        let mut cfg = test_config();
        cfg.system = vec!["You are helpful".to_string()];

        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Text,
            content: "Hello".to_string(),
            thinking: None,
            cache: None,
        }];

        let result = messages_to_text(&cfg, &messages).unwrap();
        assert_eq!(result, "You are helpful\n\nHello");
    }

    #[test]
    fn test_messages_to_text_normalizes_tool_messages() {
        let cfg = test_config();
        let tool_calls = vec![ToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "search".to_string(),
                arguments: r#"{"query": "rust"}"#.to_string(),
            },
        }];

        let messages = vec![
            ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Text,
                content: "Search for rust".to_string(),
                thinking: None,
                cache: None,
            },
            ChatMessage {
                role: ChatRole::Assistant,
                message_type: MessageType::ToolUse(tool_calls),
                content: "Searching...".to_string(),
                thinking: None,
                cache: None,
            },
        ];

        let result = messages_to_text(&cfg, &messages).unwrap();
        assert_eq!(result, "Search for rust\n\nSearching...");
    }

    #[test]
    fn test_normalize_messages_to_text() {
        let tool_calls = vec![ToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "test".to_string(),
                arguments: "{}".to_string(),
            },
        }];

        let messages = vec![
            ChatMessage {
                role: ChatRole::User,
                message_type: MessageType::Text,
                content: "Hello".to_string(),
                thinking: None,
                cache: None,
            },
            ChatMessage {
                role: ChatRole::Assistant,
                message_type: MessageType::ToolUse(tool_calls),
                content: "Using tool".to_string(),
                thinking: Some("Thinking...".to_string()),
                cache: None,
            },
        ];

        let normalized = normalize_messages_to_text(&messages);

        assert_eq!(normalized.len(), 2);
        assert!(matches!(normalized[0].message_type, MessageType::Text));
        assert!(matches!(normalized[1].message_type, MessageType::Text));
        assert_eq!(normalized[1].content, "Using tool");
        assert_eq!(normalized[1].thinking.as_ref().unwrap(), "Thinking...");
    }
}
