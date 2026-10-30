//! Wire types for Anthropic responses, stream events and errors.

use crate::wire::usage::AnthropicUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Details accompanying a non-`end_turn` stop reason (e.g. refusal category).
#[derive(Deserialize, Debug, Clone, Default)]
pub(crate) struct AnthropicStopDetails {
    #[serde(default)]
    pub(crate) category: Option<String>,
}

/// Response from Anthropic's messages API endpoint.
#[derive(Deserialize, Debug)]
pub(crate) struct AnthropicCompleteResponse {
    pub(crate) content: Vec<AnthropicContent>,
    pub(crate) stop_reason: String,
    pub(crate) usage: Option<AnthropicUsage>,
    #[serde(default)]
    pub(crate) stop_details: Option<AnthropicStopDetails>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct AnthropicStreamResponse {
    #[serde(rename = "type")]
    pub(crate) response_type: String,
    /// Index of the content block (for content_block_start, content_block_delta, content_block_stop)
    pub(crate) index: Option<usize>,
    /// Content block for content_block_start events
    pub(crate) content_block: Option<AnthropicStreamContentBlock>,
    /// Delta for content_block_delta and message_delta events
    pub(crate) delta: Option<AnthropicDelta>,
    /// Usage information (present on message_delta events; only output_tokens is populated)
    pub(crate) usage: Option<AnthropicUsage>,
    /// Nested message object (present on message_start events)
    pub(crate) message: Option<AnthropicStreamMessage>,
    /// Nested error object (present on `type: "error"` SSE events)
    pub(crate) error: Option<AnthropicApiErrorBody>,
}

/// Anthropic error envelope body (`{"type":"error","error":{...}}` or nested in SSE).
#[derive(Deserialize, Debug)]
pub(crate) struct AnthropicApiErrorBody {
    #[serde(rename = "type")]
    pub(crate) error_type: Option<String>,
    pub(crate) message: Option<String>,
}

/// Nested message object within an Anthropic `message_start` SSE event.
#[derive(Deserialize, Debug)]
pub(crate) struct AnthropicStreamMessage {
    /// Provider message id.
    #[serde(default)]
    pub(crate) id: Option<String>,
    /// Usage information present in the initial message_start event.
    pub(crate) usage: Option<AnthropicUsage>,
}

/// Content block within an Anthropic streaming content_block_start event.
#[derive(Deserialize, Debug)]
pub(crate) struct AnthropicStreamContentBlock {
    #[serde(rename = "type")]
    pub(crate) block_type: String,
    /// Tool use ID (for tool_use blocks)
    pub(crate) id: Option<String>,
    /// Tool name (for tool_use blocks)
    pub(crate) name: Option<String>,
    /// Initial text (for text blocks, usually empty)
    #[allow(dead_code)]
    pub(crate) text: Option<String>,
    /// Opaque payload (for redacted_thinking blocks)
    pub(crate) data: Option<String>,
    /// Remaining fields, kept so unmodelled block types round-trip as opaque items.
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

/// Delta content within an Anthropic streaming response.
#[derive(Deserialize, Debug)]
pub(crate) struct AnthropicDelta {
    #[serde(rename = "type")]
    pub(crate) delta_type: Option<String>,
    /// Text content (for text_delta)
    pub(crate) text: Option<String>,
    /// Partial JSON string (for input_json_delta)
    pub(crate) partial_json: Option<String>,
    /// Thinking content (for thinking_delta)
    pub(crate) thinking: Option<String>,
    /// Thinking signature (for signature_delta)
    pub(crate) signature: Option<String>,
    /// Stop reason (for message_delta)
    pub(crate) stop_reason: Option<String>,
    /// Stop details (for message_delta), e.g. refusal category
    #[serde(default)]
    pub(crate) stop_details: Option<AnthropicStopDetails>,
}

/// Content block within an Anthropic API response.
#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct AnthropicContent {
    pub(crate) text: Option<String>,
    #[serde(rename = "type")]
    pub(crate) content_type: Option<String>,
    pub(crate) thinking: Option<String>,
    /// Thinking signature (for thinking blocks)
    #[serde(default)]
    pub(crate) signature: Option<String>,
    /// Opaque payload (for redacted_thinking blocks)
    #[serde(default)]
    pub(crate) data: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) input: Option<serde_json::Value>,
    pub(crate) id: Option<String>,
}
