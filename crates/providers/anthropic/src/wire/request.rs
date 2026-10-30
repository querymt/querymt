//! Wire types for the Anthropic messages request.

use crate::thinking::{OutputConfig, ThinkingConfig};
use crate::tools::AnthropicTool;
use crate::wire::system::{AnthropicSystemPrompt, CacheControlEphemeral};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

/// Request payload for Anthropic's messages API endpoint.
#[derive(Serialize, Debug)]
pub(crate) struct AnthropicCompleteRequest<'a> {
    pub(crate) messages: Vec<AnthropicMessage>,
    pub(crate) model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<AnthropicSystemPrompt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<AnthropicTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_config: Option<OutputConfig>,
}

/// Individual message in an Anthropic chat conversation.
#[derive(Serialize, Debug)]
pub(crate) struct AnthropicMessage {
    pub(crate) role: &'static str,
    pub(crate) content: Vec<MessageContent>,
}

/// A content block sent to the Anthropic API.
///
/// Uses `#[serde(untagged)]` so each variant controls its own `type` field.
/// This avoids needing a single flat struct with many `Option` fields.
#[derive(Serialize, Debug)]
#[serde(untagged)]
pub(crate) enum MessageContent {
    Text {
        #[serde(rename = "type")]
        content_type: &'static str, // "text"
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Image {
        #[serde(rename = "type")]
        content_type: &'static str, // "image"
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    ImageUrl {
        #[serde(rename = "type")]
        content_type: &'static str, // "image_url"
        image_url: ImageUrlContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Document {
        #[serde(rename = "type")]
        content_type: &'static str, // "document"
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Thinking {
        #[serde(rename = "type")]
        content_type: &'static str, // "thinking"
        thinking: String,
        signature: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    RedactedThinking {
        #[serde(rename = "type")]
        content_type: &'static str, // "redacted_thinking"
        data: String,
    },
    ToolUse {
        #[serde(rename = "type")]
        content_type: &'static str, // "tool_use"
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    ToolResult {
        #[serde(rename = "type")]
        content_type: &'static str, // "tool_result"
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        content: Vec<ToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
}

/// Content block inside a tool_result (Anthropic supports text + image).
#[derive(Serialize, Debug)]
#[serde(untagged)]
pub(crate) enum ToolResultContent {
    Text {
        #[serde(rename = "type")]
        content_type: &'static str,
        text: String,
    },
    Image {
        #[serde(rename = "type")]
        content_type: &'static str,
        source: ImageSource,
    },
}

impl MessageContent {
    pub(crate) fn set_cache_control(&mut self, cc: CacheControlEphemeral) {
        match self {
            MessageContent::Text { cache_control, .. }
            | MessageContent::Image { cache_control, .. }
            | MessageContent::ImageUrl { cache_control, .. }
            | MessageContent::Document { cache_control, .. }
            | MessageContent::Thinking { cache_control, .. }
            | MessageContent::ToolUse { cache_control, .. }
            | MessageContent::ToolResult { cache_control, .. } => {
                *cache_control = Some(cc);
            }
            // `redacted_thinking` blocks accept no cache_control.
            MessageContent::RedactedThinking { .. } => {}
        }
    }
}

#[derive(Serialize, Debug)]
pub(crate) struct ImageUrlContent {
    pub(crate) url: String,
}

#[derive(Serialize, Debug)]
pub(crate) struct ImageSource {
    #[serde(rename = "type")]
    pub(crate) source_type: &'static str,
    pub(crate) media_type: String,
    pub(crate) data: String,
}
