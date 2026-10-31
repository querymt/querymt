//! Per-block accumulators for in-flight streamed content blocks.

use crate::response::{text_message_item, thinking_item};
use querymt::chat::{
    ChatFunctionCallItem, ChatOpaqueItem, ChatOutputItem, INVALID_INPUT_EXTENSION,
};
use serde_json::Value;

/// Per-block accumulator used while streaming tool-use content.
///
/// Anthropic's SSE protocol emits tool arguments as a sequence of
/// `input_json_delta` events followed by a `content_block_stop`.  We
/// accumulate the partial JSON here and emit the authoritative function-call
/// snapshot when the block closes.
#[derive(Debug, Default)]
pub(crate) struct AnthropicToolUseState {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments_buffer: String,
}

impl AnthropicToolUseState {
    /// The function-call item as announced at `content_block_start`.
    pub(crate) fn started_item(&self) -> ChatOutputItem {
        ChatOutputItem::FunctionCall(ChatFunctionCallItem {
            item_id: None,
            call_id: self.id.clone(),
            name: self.name.clone(),
            arguments: String::new(),
            status: None,
            extensions: Default::default(),
        })
    }

    /// Complete this block. `interrupted` is set when the response was cut off
    /// while the block was still open. The raw buffer is preserved verbatim; only
    /// a normally completed block with no input deltas becomes `{}`.
    pub(crate) fn into_item(self, interrupted: bool) -> ChatOutputItem {
        let invalid = interrupted
            || (!self.arguments_buffer.is_empty()
                && !matches!(
                    serde_json::from_str::<Value>(&self.arguments_buffer),
                    Ok(Value::Object(_))
                ));
        let arguments = if self.arguments_buffer.is_empty() && !interrupted {
            "{}".to_string()
        } else {
            self.arguments_buffer
        };
        let mut extensions = querymt::chat::Extensions::new();
        if invalid {
            extensions.insert(INVALID_INPUT_EXTENSION.to_string(), Value::Bool(true));
        }
        ChatOutputItem::FunctionCall(ChatFunctionCallItem {
            item_id: None,
            call_id: self.id,
            name: self.name,
            arguments,
            status: None,
            extensions,
        })
    }
}

/// Per-block state of an in-flight streamed content block, keyed by block index.
#[derive(Debug)]
pub(crate) enum AnthropicBlockState {
    Text {
        text: String,
    },
    /// Created at block start so an empty block (display "omitted") still
    /// carries its signature in the completion snapshot.
    Thinking {
        text: String,
        signature: String,
    },
    Tool(AnthropicToolUseState),
    /// Any block type this provider does not model. Deltas are buffered here
    /// because no structured delta targets opaque items.
    Opaque {
        original_type: String,
        payload: serde_json::Map<String, Value>,
        /// Raw `input_json_delta` text, recorded on the payload at stop.
        input_json: String,
    },
}

impl AnthropicBlockState {
    /// Block kind for diagnostics.
    pub(crate) fn kind(&self) -> &str {
        match self {
            Self::Text { .. } => "text",
            Self::Thinking { .. } => "thinking",
            Self::Tool(_) => "tool_use",
            Self::Opaque { original_type, .. } => original_type,
        }
    }

    /// Authoritative snapshot of this block at `content_block_stop`.
    pub(crate) fn into_item(self, interrupted: bool) -> ChatOutputItem {
        match self {
            Self::Text { text } => text_message_item(text),
            Self::Thinking { text, signature } => thinking_item(text, signature),
            Self::Tool(state) => state.into_item(interrupted),
            Self::Opaque {
                original_type,
                mut payload,
                input_json,
            } => {
                // Match the non-streaming block shape: parsed object input
                // replaces `input`; anything else is kept verbatim and then
                // takes precedence over `input`.
                if !input_json.is_empty() {
                    match serde_json::from_str::<Value>(&input_json) {
                        Ok(input @ Value::Object(_)) => {
                            payload.insert("input".into(), input);
                        }
                        _ => {
                            payload.insert("input_json".into(), Value::String(input_json));
                        }
                    }
                }
                ChatOutputItem::Opaque(ChatOpaqueItem {
                    original_type,
                    payload: Value::Object(payload),
                })
            }
        }
    }
}
