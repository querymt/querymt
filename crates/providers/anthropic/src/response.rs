//! Converting non-streaming responses into canonical output.

use crate::config::Anthropic;
use crate::wire::response::{AnthropicCompleteResponse, AnthropicStopDetails};
use querymt::Usage;
use querymt::chat::{
    ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem,
    ChatOutputStatus, ChatReasoningItem, ChatReasoningPart, ChatRole, FinishReason,
};
use serde_json::Value;

/// Assistant message item holding one text part.
pub(crate) fn text_message_item(text: String) -> ChatOutputItem {
    ChatOutputItem::Message(ChatMessageItem {
        id: None,
        role: ChatRole::Assistant,
        phase: None,
        status: None,
        parts: vec![ChatMessagePart::Text {
            text,
            annotations: Vec::new(),
            extensions: Default::default(),
        }],
        extensions: Default::default(),
    })
}

/// Reasoning item for a `thinking` block; an empty signature is absent.
pub(crate) fn thinking_item(text: String, signature: String) -> ChatOutputItem {
    ChatOutputItem::Reasoning(ChatReasoningItem {
        id: None,
        summary: Vec::new(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ChatReasoningPart::text(text)]
        },
        encrypted_content: None,
        signature: Some(signature).filter(|s| !s.is_empty()),
        status: None,
        extensions: Default::default(),
    })
}

impl std::fmt::Display for AnthropicCompleteResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for content in self.content.iter() {
            match content.content_type {
                Some(ref t) if t == "tool_use" => write!(
                    f,
                    "{{\n \"name\": {}, \"input\": {}\n}}",
                    content.name.clone().unwrap_or_default(),
                    content.input.clone().unwrap_or(serde_json::Value::Null)
                )?,
                Some(ref t) if t == "thinking" => {
                    write!(f, "{}", content.thinking.clone().unwrap_or_default())?
                }
                _ => write!(
                    f,
                    "{}",
                    self.content
                        .iter()
                        .map(|c| c.text.clone().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n")
                )?,
            }
        }
        Ok(())
    }
}

impl From<AnthropicCompleteResponse> for ChatOutput {
    fn from(response: AnthropicCompleteResponse) -> Self {
        let mut items: Vec<ChatOutputItem> = Vec::new();

        // Build items in content order so thinking blocks keep their own
        // signature and position relative to text and tool calls.
        for block in &response.content {
            match block.content_type.as_deref() {
                Some("thinking") => {
                    let text = block.thinking.clone().unwrap_or_default();
                    let signature = block.signature.clone().filter(|s| !s.is_empty());
                    if text.is_empty() && signature.is_none() {
                        continue;
                    }
                    items.push(ChatOutputItem::Reasoning(ChatReasoningItem {
                        id: None,
                        summary: Vec::new(),
                        content: if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![ChatReasoningPart::text(text)]
                        },
                        encrypted_content: None,
                        signature,
                        status: None,
                        extensions: Default::default(),
                    }));
                }
                Some("redacted_thinking") => {
                    if let Some(data) = block.data.clone() {
                        items.push(redacted_reasoning_item(data));
                    }
                }
                Some("tool_use") => {
                    items.push(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                        item_id: None,
                        call_id: block.id.clone().unwrap_or_default(),
                        name: block.name.clone().unwrap_or_default(),
                        arguments: serde_json::to_string(
                            &block.input.clone().unwrap_or(serde_json::Value::Null),
                        )
                        .unwrap_or_default(),
                        status: None,
                        extensions: Default::default(),
                    }));
                }
                Some("text") | None => {
                    let Some(text) = block.text.clone().filter(|t| !t.is_empty()) else {
                        continue;
                    };
                    // Consecutive text blocks form one message.
                    if let Some(ChatOutputItem::Message(message)) = items.last_mut()
                        && let Some(ChatMessagePart::Text { text: existing, .. }) =
                            message.parts.last_mut()
                    {
                        existing.push('\n');
                        existing.push_str(&text);
                        continue;
                    }
                    items.push(ChatOutputItem::Message(ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![ChatMessagePart::Text {
                            text,
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    }));
                }
                Some(_) => {}
            }
        }

        let finish_reason = Anthropic::map_stop_reason_with_details(
            &response.stop_reason,
            response.stop_details.as_ref(),
        );

        ChatOutput {
            items,
            status: Some(ChatOutputStatus::Completed),
            usage: response.usage.map(Usage::from),
            finish_reason: Some(finish_reason),
            ..ChatOutput::default()
        }
    }
}

/// Build the reasoning item that carries a `redacted_thinking` payload.
pub(crate) fn redacted_reasoning_item(data: String) -> ChatOutputItem {
    let mut extensions = querymt::chat::Extensions::new();
    extensions.insert(
        querymt::chat::ANTHROPIC_BLOCK_EXTENSION.to_string(),
        Value::String(querymt::chat::ANTHROPIC_REDACTED_THINKING.to_string()),
    );
    ChatOutputItem::Reasoning(ChatReasoningItem {
        id: None,
        summary: Vec::new(),
        content: Vec::new(),
        encrypted_content: Some(data),
        signature: None,
        status: None,
        extensions,
    })
}

impl Anthropic {
    /// Map a raw Anthropic `stop_reason` string to a typed `FinishReason`.
    ///
    /// This is the single mapping used by both the streaming and
    /// non-streaming paths.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn map_stop_reason(stop_reason: &str) -> FinishReason {
        Self::map_stop_reason_with_details(stop_reason, None)
    }

    /// Like [`map_stop_reason`], additionally logging the raw reason and, for
    /// refusals, the reported `stop_details.category`.
    pub(crate) fn map_stop_reason_with_details(
        stop_reason: &str,
        details: Option<&AnthropicStopDetails>,
    ) -> FinishReason {
        let finish_reason = match stop_reason {
            "end_turn" | "stop_sequence" => FinishReason::Stop,
            "tool_use" => FinishReason::ToolCalls,
            "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
            "refusal" => FinishReason::ContentFilter,
            "pause_turn" => FinishReason::Other,
            _ => FinishReason::Unknown,
        };
        log::debug!(target: "qmt_anthropic", "stop_reason={stop_reason:?} -> {finish_reason:?}");
        if stop_reason == "refusal"
            && let Some(category) = details.and_then(|d| d.category.as_deref())
        {
            log::info!(target: "qmt_anthropic", "refusal stop_details.category={category}");
        }
        finish_reason
    }
}
