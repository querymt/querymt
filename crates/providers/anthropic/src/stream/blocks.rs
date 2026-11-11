//! Handling of `content_block_start` and content-block deltas.

use crate::config::Anthropic;
use crate::response::{redacted_reasoning_item, text_message_item, thinking_item};
use crate::stream::state::{AnthropicBlockState, AnthropicToolUseState};
use crate::stream::{AnthropicStreamParser, structured};
use crate::wire::response::{AnthropicDelta, AnthropicStreamContentBlock};
use querymt::chat::{
    ChatMessagePart, ChatMessagePartDelta, ChatOpaqueItem, ChatOutputItem, ReasoningPartKind,
    StructuredStreamEvent,
};
use serde_json::Value;

impl AnthropicStreamParser {
    pub(crate) fn start_block(
        &mut self,
        index: usize,
        block: AnthropicStreamContentBlock,
        chunks: &mut Vec<querymt::chat::StreamChunk>,
    ) {
        match block.block_type.as_str() {
            "text" => {
                let initial = block.text.unwrap_or_default();
                chunks.push(structured(StructuredStreamEvent::ItemStarted {
                    output_index: index,
                    item: text_message_item(String::new()),
                }));
                chunks.push(structured(StructuredStreamEvent::MessagePartStarted {
                    output_index: index,
                    content_index: 0,
                    part: ChatMessagePart::Text {
                        text: String::new(),
                        annotations: Vec::new(),
                        extensions: Default::default(),
                    },
                }));
                if !initial.is_empty() {
                    chunks.push(structured(StructuredStreamEvent::MessagePartDelta {
                        output_index: index,
                        content_index: 0,
                        delta: ChatMessagePartDelta::Text {
                            delta: initial.clone(),
                        },
                    }));
                }
                self.blocks
                    .insert(index, AnthropicBlockState::Text { text: initial });
            }
            "thinking" => {
                // Created at block start so an empty block (display "omitted")
                // still carries its signature in the completion snapshot.
                chunks.push(structured(StructuredStreamEvent::ItemStarted {
                    output_index: index,
                    item: thinking_item(String::new(), String::new()),
                }));
                chunks.push(structured(StructuredStreamEvent::ReasoningPartStarted {
                    output_index: index,
                    part: ReasoningPartKind::Content,
                    part_index: 0,
                }));
                self.blocks.insert(
                    index,
                    AnthropicBlockState::Thinking {
                        text: String::new(),
                        signature: String::new(),
                    },
                );
            }
            "redacted_thinking" => {
                if let Some(data) = block.data.filter(|d| !d.is_empty()) {
                    let item = redacted_reasoning_item(data);
                    chunks.push(structured(StructuredStreamEvent::ItemStarted {
                        output_index: index,
                        item: item.clone(),
                    }));
                    chunks.push(structured(StructuredStreamEvent::ItemCompleted {
                        output_index: index,
                        item,
                    }));
                }
            }
            "tool_use" => {
                let name = block.name.unwrap_or_default();
                let name = if self.oauth {
                    Anthropic::strip_tool_prefix(&name, Some(&self.tool_names))
                } else {
                    name
                };
                let state = AnthropicToolUseState {
                    id: block.id.unwrap_or_default(),
                    name,
                    arguments_buffer: String::new(),
                };
                chunks.push(structured(StructuredStreamEvent::ItemStarted {
                    output_index: index,
                    item: state.started_item(),
                }));
                self.blocks.insert(index, AnthropicBlockState::Tool(state));
            }
            other => {
                // Unmodelled block (e.g. `server_tool_use`): retained as a
                // non-executable opaque item recording the start payload.
                let original_type = other.to_string();
                let mut payload = block.extra;
                payload.insert("type".into(), Value::String(original_type.clone()));
                for (key, value) in [
                    ("id", block.id),
                    ("name", block.name),
                    ("text", block.text),
                    ("data", block.data),
                ] {
                    if let Some(value) = value {
                        payload.insert(key.into(), Value::String(value));
                    }
                }
                chunks.push(structured(StructuredStreamEvent::ItemStarted {
                    output_index: index,
                    item: ChatOutputItem::Opaque(ChatOpaqueItem {
                        original_type: original_type.clone(),
                        payload: Value::Object(payload.clone()),
                    }),
                }));
                self.blocks.insert(
                    index,
                    AnthropicBlockState::Opaque {
                        original_type,
                        payload,
                        input_json: String::new(),
                    },
                );
            }
        }
    }

    pub(crate) fn apply_delta(
        &mut self,
        index: usize,
        delta: AnthropicDelta,
        chunks: &mut Vec<querymt::chat::StreamChunk>,
    ) {
        let Some(state) = self.blocks.get_mut(&index) else {
            // Index only: delta content is never logged.
            log::warn!("Anthropic stream delta for content block {index} without a start");
            return;
        };
        let delta_type = delta.delta_type.clone();
        let consumed = match state {
            AnthropicBlockState::Text { text } => {
                let consumed = delta.text.is_some();
                if let Some(delta) = delta.text.filter(|d| !d.is_empty()) {
                    text.push_str(&delta);
                    chunks.push(structured(StructuredStreamEvent::MessagePartDelta {
                        output_index: index,
                        content_index: 0,
                        delta: ChatMessagePartDelta::Text { delta },
                    }));
                }
                consumed
            }
            AnthropicBlockState::Thinking { text, signature } => {
                let consumed = delta.thinking.is_some() || delta.signature.is_some();
                if let Some(delta) = delta.thinking.filter(|d| !d.is_empty()) {
                    text.push_str(&delta);
                    chunks.push(structured(StructuredStreamEvent::ReasoningPartDelta {
                        output_index: index,
                        part: ReasoningPartKind::Content,
                        part_index: 0,
                        delta,
                    }));
                }
                if let Some(delta) = delta.signature {
                    signature.push_str(&delta);
                }
                consumed
            }
            AnthropicBlockState::Tool(state) => {
                let consumed = delta.partial_json.is_some();
                if let Some(delta) = delta.partial_json.filter(|d| !d.is_empty()) {
                    state.arguments_buffer.push_str(&delta);
                    chunks.push(structured(StructuredStreamEvent::FunctionArgumentsDelta {
                        output_index: index,
                        delta,
                    }));
                }
                consumed
            }
            AnthropicBlockState::Opaque {
                payload,
                input_json,
                ..
            } => {
                // No structured delta targets opaque items: buffer here.
                // Other delta types are dropped, as in non-streaming.
                if let Some(delta) = delta.partial_json {
                    input_json.push_str(&delta);
                }
                if let Some(delta) = delta.text {
                    match payload.get_mut("text") {
                        Some(Value::String(existing)) => existing.push_str(&delta),
                        _ => {
                            payload.insert("text".into(), Value::String(delta));
                        }
                    }
                }
                true
            }
        };
        if !consumed {
            // Index, block kind and delta type only; never content.
            log::debug!(
                target: "qmt_anthropic",
                "ignored {delta_type:?} delta for {} block {index}",
                state.kind()
            );
        }
    }
}
