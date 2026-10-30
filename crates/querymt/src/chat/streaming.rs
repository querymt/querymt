use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    ChatFunctionCallItem, ChatMessagePart, ChatOutput, ChatOutputItem, ChatOutputProvenance,
    ChatOutputStatus, ChatReasoningPart, FinishReason, StreamChunk, empty_message_part,
};
use crate::Usage;

/// Terminal detail reported when a reply is cut off by the output token limit
/// while tool calls are still being streamed.
pub const LENGTH_CUTOFF_DETAIL: &str = "output token limit";

/// Item-aware stream events. Providers emit metadata before semantic events to declare
/// structured mode. Compatibility projections are produced only at explicit boundaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StructuredStreamEvent {
    ResponseMetadata {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<ChatOutputStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provenance: Option<ChatOutputProvenance>,
    },
    ItemStarted {
        output_index: usize,
        item: ChatOutputItem,
    },
    /// A new indexed message content part was opened by the provider.
    ///
    /// Providers emit this for `response.content_part.added` so that a delta
    /// can address a part that already exists, rather than requiring the
    /// accumulator to synthesize it from an item snapshot that omitted it.
    MessagePartStarted {
        output_index: usize,
        content_index: usize,
        part: ChatMessagePart,
    },
    MessagePartDelta {
        output_index: usize,
        content_index: usize,
        delta: ChatMessagePartDelta,
    },
    /// A new indexed reasoning part was opened by the provider.
    ReasoningPartStarted {
        output_index: usize,
        part: ReasoningPartKind,
        part_index: usize,
    },
    ReasoningPartDelta {
        output_index: usize,
        part: ReasoningPartKind,
        part_index: usize,
        delta: String,
    },
    FunctionArgumentsDelta {
        output_index: usize,
        delta: String,
    },
    ItemCompleted {
        output_index: usize,
        item: ChatOutputItem,
    },
    /// Semantic response terminal. Transport framing markers do not map to this event.
    ResponseTerminal {
        status: ChatOutputStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatMessagePartDelta {
    Text { delta: String },
    Refusal { delta: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPartKind {
    Summary,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccumulationMode {
    Undecided,
    Legacy,
    Structured,
}

/// Explicit outcome of finalizing a canonical accumulation attempt.
///
/// Every variant carries the available canonical output, so partial items from
/// incomplete or failed responses remain inspectable. Only `Completed` output is
/// authorized for local tool execution; unfinished calls in the other variants
/// are never executable.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatStreamFinish {
    /// The response completed successfully.
    Completed(ChatOutput),
    /// The provider reported an incomplete response (for example an output
    /// token limit). Partial output is retained with the terminal detail.
    Incomplete {
        output: ChatOutput,
        detail: Option<String>,
    },
    /// The response failed, or the stream ended without a valid terminal.
    Failed {
        output: ChatOutput,
        error: ChatStreamAccumulatorError,
    },
}

impl ChatStreamFinish {
    /// Borrow the available canonical output, whether complete or partial.
    pub fn output(&self) -> &ChatOutput {
        match self {
            ChatStreamFinish::Completed(output)
            | ChatStreamFinish::Incomplete { output, .. }
            | ChatStreamFinish::Failed { output, .. } => output,
        }
    }

    /// Consume the outcome, returning the available canonical output.
    pub fn into_output(self) -> ChatOutput {
        match self {
            ChatStreamFinish::Completed(output)
            | ChatStreamFinish::Incomplete { output, .. }
            | ChatStreamFinish::Failed { output, .. } => output,
        }
    }

    /// Whether the response completed successfully.
    pub fn is_completed(&self) -> bool {
        matches!(self, ChatStreamFinish::Completed(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatStreamAccumulatorError {
    #[error("structured response metadata must precede semantic output")]
    LateStructuredMetadata,
    #[error("structured item delta references missing output index {0}")]
    MissingItem(usize),
    #[error("structured message delta references non-message output index {0}")]
    ExpectedMessage(usize),
    #[error(
        "structured message part at output index {output_index}, content index {content_index} was started more than once"
    )]
    ConflictingPartStart {
        output_index: usize,
        content_index: usize,
    },
    #[error(
        "structured message delta type does not match output index {output_index}, content index {content_index}"
    )]
    MessagePartTypeMismatch {
        output_index: usize,
        content_index: usize,
    },
    #[error("structured reasoning delta references non-reasoning output index {0}")]
    ExpectedReasoning(usize),
    #[error("structured argument delta references non-function output index {0}")]
    ExpectedFunctionCall(usize),
    #[error("structured item at output index {output_index} changed identity")]
    ConflictingItemIdentity { output_index: usize },
    #[error(
        "structured item at output index {output_index} completed more than once with conflicting snapshots"
    )]
    ConflictingCompletion { output_index: usize },
    #[error("structured item at output index {output_index} received data after completion")]
    EventAfterItemCompletion { output_index: usize },
    #[error("stream received output after its terminal event")]
    EventAfterTerminal,
    #[error("stream ended before a semantic response terminal")]
    PrematureEof,
    #[error("completed response has unfinished output indexes: {0:?}")]
    IncompleteItems(Vec<usize>),
    #[error("completed legacy response has unfinished tool call indexes: {0:?}")]
    IncompleteToolCalls(Vec<usize>),
    #[error("response was incomplete{detail}", detail = format_detail(.0))]
    IncompleteResponse(Option<String>),
    #[error("response failed{detail}", detail = format_detail(.0))]
    FailedResponse(Option<String>),
    #[error("response terminal retained non-terminal status {0:?}")]
    InvalidTerminalStatus(ChatOutputStatus),
}

fn format_detail(detail: &Option<String>) -> String {
    detail
        .as_deref()
        .map(|detail| format!(": {detail}"))
        .unwrap_or_default()
}

/// Shared accumulator for legacy and item-aware streams.
#[derive(Debug, Clone)]
pub struct ChatStreamAccumulator {
    mode: AccumulationMode,
    output: ChatOutput,
    structured_items: BTreeMap<usize, ChatOutputItem>,
    completed_items: BTreeSet<usize>,
    pending_legacy_calls: BTreeSet<usize>,
    /// Every flat-chunk call index that received a start this attempt, so a
    /// delta after completion is not mistaken for a delta without a start.
    started_legacy_calls: BTreeSet<usize>,
    /// Orphan delta indexes already reported, so each warns once per attempt.
    warned_orphan_calls: BTreeSet<usize>,
    terminal_detail: Option<String>,
    terminal_seen: bool,
}

impl Default for ChatStreamAccumulator {
    fn default() -> Self {
        Self {
            mode: AccumulationMode::Undecided,
            output: ChatOutput::default(),
            structured_items: BTreeMap::new(),
            completed_items: BTreeSet::new(),
            pending_legacy_calls: BTreeSet::new(),
            started_legacy_calls: BTreeSet::new(),
            warned_orphan_calls: BTreeSet::new(),
            terminal_detail: None,
            terminal_seen: false,
        }
    }
}

impl ChatStreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_structured(&self) -> bool {
        self.mode == AccumulationMode::Structured
    }

    pub fn push(&mut self, chunk: &StreamChunk) -> Result<(), ChatStreamAccumulatorError> {
        if self.terminal_seen {
            return Err(ChatStreamAccumulatorError::EventAfterTerminal);
        }
        match chunk {
            StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
                response_id,
                status,
                usage,
                finish_reason,
                provenance,
            }) => {
                if self.mode == AccumulationMode::Legacy {
                    return Err(ChatStreamAccumulatorError::LateStructuredMetadata);
                }
                self.mode = AccumulationMode::Structured;
                self.output.response_id.clone_from(response_id);
                self.output.status = *status;
                self.output.usage.clone_from(usage);
                self.output.finish_reason = *finish_reason;
                self.output.provenance.clone_from(provenance);
            }
            StreamChunk::Structured(event) => {
                if self.mode != AccumulationMode::Structured {
                    return Err(ChatStreamAccumulatorError::LateStructuredMetadata);
                }
                self.push_structured(event)?;
            }
            _ if self.mode == AccumulationMode::Structured => {
                // Legacy chunks accompanying structured events are compatibility projections.
            }
            StreamChunk::Text(delta) => {
                self.select_legacy();
                append_legacy_message_text(&mut self.output.items, delta);
            }
            StreamChunk::Thinking(delta) => {
                self.select_legacy();
                append_legacy_reasoning(&mut self.output.items, delta);
            }
            StreamChunk::ThinkingSignature(signature) => {
                self.select_legacy();
                append_legacy_signature(&mut self.output.items, signature);
            }
            StreamChunk::RedactedThinking(data) => {
                self.select_legacy();
                append_legacy_redacted(&mut self.output.items, data);
            }
            StreamChunk::ToolUseComplete {
                index,
                tool_call,
                extensions,
            } => {
                self.select_legacy();
                self.pending_legacy_calls.remove(index);
                self.output
                    .items
                    .push(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                        item_id: None,
                        call_id: tool_call.id.clone(),
                        name: tool_call.function.name.clone(),
                        arguments: tool_call.function.arguments.clone(),
                        status: Some(ChatOutputStatus::Completed),
                        extensions: extensions.clone(),
                    }));
            }
            StreamChunk::Usage(usage) => {
                self.select_legacy();
                self.output.usage = Some(match self.output.usage.take() {
                    Some(previous) => previous.merge_max(usage.clone()),
                    None => usage.clone(),
                });
            }
            StreamChunk::Done { finish_reason } => {
                self.select_legacy();
                self.output.status = Some(ChatOutputStatus::Completed);
                self.output.finish_reason = Some(*finish_reason);
                self.terminal_seen = true;
            }
            StreamChunk::ToolUseStart { index, .. } => {
                self.select_legacy();
                self.started_legacy_calls.insert(*index);
                self.pending_legacy_calls.insert(*index);
            }
            StreamChunk::ToolUseInputDelta { index, .. } => {
                self.select_legacy();
                // Only an explicit start opens a pending call. A delta for an
                // index that was never started cannot become an executable
                // call, so it is reported (index only, never argument text)
                // and otherwise ignored, once per index.
                if self.pending_legacy_calls.contains(index) {
                    // Normal delta for an open call.
                } else if self.started_legacy_calls.contains(index) {
                    log::debug!("tool call argument delta for index {index} after completion");
                } else if self.warned_orphan_calls.insert(*index) {
                    log::warn!(
                        "provider contract violation: tool call argument delta for index {index} without a start"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn output(&self) -> ChatOutput {
        let mut output = self.output.clone();
        if self.mode == AccumulationMode::Structured {
            output.items = self.structured_items.values().cloned().collect();
        }
        output
    }

    /// Consume the accumulator and finalize into an explicit outcome.
    ///
    /// Finalization consumes the accumulator so callers cannot keep appending
    /// after interpreting a terminal outcome. Successful completion carries the
    /// canonical output; incomplete and failed outcomes retain the available
    /// partial output alongside the classified terminal cause so partial items
    /// remain inspectable while unfinished calls are never executable.
    pub fn finish(mut self) -> ChatStreamFinish {
        let unfinished = if self.mode == AccumulationMode::Structured {
            self.structured_items
                .keys()
                .filter(|index| !self.completed_items.contains(index))
                .copied()
                .collect()
        } else {
            Vec::new()
        };
        let incomplete_legacy_calls: Vec<usize> =
            self.pending_legacy_calls.iter().copied().collect();
        if self.mode == AccumulationMode::Structured {
            self.output.items = self.structured_items.into_values().collect();
        }
        let status = self.output.status;
        let terminal_detail = self.terminal_detail;
        let output = self.output;

        match status {
            Some(ChatOutputStatus::Completed) => {
                if !incomplete_legacy_calls.is_empty() {
                    // A length stop inside a tool call is an output-limit
                    // cut-off, not a transient stream failure: report it as
                    // incomplete so it is never retried as-is.
                    if output.finish_reason == Some(FinishReason::Length) {
                        let mut output = output;
                        output.status = Some(ChatOutputStatus::Incomplete);
                        return ChatStreamFinish::Incomplete {
                            output,
                            detail: Some(LENGTH_CUTOFF_DETAIL.to_string()),
                        };
                    }
                    return ChatStreamFinish::Failed {
                        error: ChatStreamAccumulatorError::IncompleteToolCalls(
                            incomplete_legacy_calls,
                        ),
                        output,
                    };
                }
                if !unfinished.is_empty() {
                    return ChatStreamFinish::Failed {
                        error: ChatStreamAccumulatorError::IncompleteItems(unfinished),
                        output,
                    };
                }
                ChatStreamFinish::Completed(output)
            }
            Some(ChatOutputStatus::Incomplete) => ChatStreamFinish::Incomplete {
                output,
                detail: terminal_detail,
            },
            Some(ChatOutputStatus::Failed) => ChatStreamFinish::Failed {
                output,
                error: ChatStreamAccumulatorError::FailedResponse(terminal_detail),
            },
            Some(status) => ChatStreamFinish::Failed {
                output,
                error: ChatStreamAccumulatorError::InvalidTerminalStatus(status),
            },
            None => {
                // An unfinished local-call set is a distinct terminal cause.
                if !incomplete_legacy_calls.is_empty() {
                    return ChatStreamFinish::Failed {
                        output,
                        error: ChatStreamAccumulatorError::IncompleteToolCalls(
                            incomplete_legacy_calls,
                        ),
                    };
                }
                ChatStreamFinish::Failed {
                    output,
                    error: ChatStreamAccumulatorError::PrematureEof,
                }
            }
        }
    }

    /// Finalize and require successful completion, discarding partial output on
    /// incomplete or failed outcomes.
    pub fn finish_success(self) -> Result<ChatOutput, ChatStreamAccumulatorError> {
        match self.finish() {
            ChatStreamFinish::Completed(output) => Ok(output),
            ChatStreamFinish::Incomplete { detail, .. } => {
                Err(ChatStreamAccumulatorError::IncompleteResponse(detail))
            }
            ChatStreamFinish::Failed { error, .. } => Err(error),
        }
    }

    /// Start an isolated physical request attempt, discarding all prior partial state.
    pub fn reset_attempt(&mut self) {
        *self = Self::default();
    }

    fn select_legacy(&mut self) {
        if self.mode == AccumulationMode::Undecided {
            self.mode = AccumulationMode::Legacy;
        }
    }

    fn push_structured(
        &mut self,
        event: &StructuredStreamEvent,
    ) -> Result<(), ChatStreamAccumulatorError> {
        match event {
            StructuredStreamEvent::ResponseMetadata { .. } => {}
            StructuredStreamEvent::ItemStarted { output_index, item } => {
                self.ensure_item_open(*output_index)?;
                if let Some(existing) = self.structured_items.get(output_index)
                    && !same_item_identity(existing, item)
                {
                    return Err(ChatStreamAccumulatorError::ConflictingItemIdentity {
                        output_index: *output_index,
                    });
                }
                self.structured_items.insert(*output_index, item.clone());
            }
            StructuredStreamEvent::ItemCompleted { output_index, item } => {
                if let Some(existing) = self.structured_items.get(output_index)
                    && !same_item_identity(existing, item)
                {
                    return Err(ChatStreamAccumulatorError::ConflictingItemIdentity {
                        output_index: *output_index,
                    });
                }
                if self.completed_items.contains(output_index) {
                    if self.structured_items.get(output_index) != Some(item) {
                        return Err(ChatStreamAccumulatorError::ConflictingCompletion {
                            output_index: *output_index,
                        });
                    }
                    return Ok(());
                }
                self.structured_items.insert(*output_index, item.clone());
                self.completed_items.insert(*output_index);
            }
            StructuredStreamEvent::MessagePartStarted {
                output_index,
                content_index,
                part,
            } => {
                self.ensure_item_open(*output_index)?;
                let item = self
                    .structured_items
                    .get_mut(output_index)
                    .ok_or(ChatStreamAccumulatorError::MissingItem(*output_index))?;
                let ChatOutputItem::Message(message) = item else {
                    return Err(ChatStreamAccumulatorError::ExpectedMessage(*output_index));
                };
                // Extend with placeholders so an out-of-order or skipped
                // part-added event cannot leave a hole; the declared part is
                // then written at its declared index.
                while message.parts.len() <= *content_index {
                    message.parts.push(empty_message_part());
                }
                if message.parts.len() > *content_index + 1 {
                    return Err(ChatStreamAccumulatorError::ConflictingPartStart {
                        output_index: *output_index,
                        content_index: *content_index,
                    });
                }
                message.parts[*content_index] = part.clone();
            }
            StructuredStreamEvent::MessagePartDelta {
                output_index,
                content_index,
                delta,
            } => {
                self.ensure_item_open(*output_index)?;
                let item = self
                    .structured_items
                    .get_mut(output_index)
                    .ok_or(ChatStreamAccumulatorError::MissingItem(*output_index))?;
                let ChatOutputItem::Message(message) = item else {
                    return Err(ChatStreamAccumulatorError::ExpectedMessage(*output_index));
                };
                // Create the indexed part on demand when the provider streamed
                // a delta without a preceding part-added event.
                while message.parts.len() <= *content_index {
                    message.parts.push(empty_message_part());
                }
                let part = &mut message.parts[*content_index];
                match (part, delta) {
                    (ChatMessagePart::Text { text, .. }, ChatMessagePartDelta::Text { delta }) => {
                        text.push_str(delta);
                    }
                    (
                        ChatMessagePart::Refusal { refusal, .. },
                        ChatMessagePartDelta::Refusal { delta },
                    ) => refusal.push_str(delta),
                    (placeholder, delta) if placeholder.is_empty_placeholder() => {
                        *placeholder = placeholder.clone().into_message_part(delta);
                    }
                    _ => {
                        return Err(ChatStreamAccumulatorError::MessagePartTypeMismatch {
                            output_index: *output_index,
                            content_index: *content_index,
                        });
                    }
                }
            }
            StructuredStreamEvent::ReasoningPartStarted {
                output_index,
                part,
                part_index,
            } => {
                self.ensure_item_open(*output_index)?;
                let item = self
                    .structured_items
                    .get_mut(output_index)
                    .ok_or(ChatStreamAccumulatorError::MissingItem(*output_index))?;
                let ChatOutputItem::Reasoning(reasoning) = item else {
                    return Err(ChatStreamAccumulatorError::ExpectedReasoning(*output_index));
                };
                let parts = match part {
                    ReasoningPartKind::Summary => &mut reasoning.summary,
                    ReasoningPartKind::Content => &mut reasoning.content,
                };
                while parts.len() <= *part_index {
                    parts.push(ChatReasoningPart::text(String::new()));
                }
                parts[*part_index].text.clear();
            }
            StructuredStreamEvent::ReasoningPartDelta {
                output_index,
                part,
                part_index,
                delta,
            } => {
                self.ensure_item_open(*output_index)?;
                let item = self
                    .structured_items
                    .get_mut(output_index)
                    .ok_or(ChatStreamAccumulatorError::MissingItem(*output_index))?;
                let ChatOutputItem::Reasoning(reasoning) = item else {
                    return Err(ChatStreamAccumulatorError::ExpectedReasoning(*output_index));
                };
                let parts = match part {
                    ReasoningPartKind::Summary => &mut reasoning.summary,
                    ReasoningPartKind::Content => &mut reasoning.content,
                };
                while parts.len() <= *part_index {
                    parts.push(ChatReasoningPart::text(String::new()));
                }
                parts[*part_index].text.push_str(delta);
            }
            StructuredStreamEvent::FunctionArgumentsDelta {
                output_index,
                delta,
            } => {
                self.ensure_item_open(*output_index)?;
                let item = self
                    .structured_items
                    .get_mut(output_index)
                    .ok_or(ChatStreamAccumulatorError::MissingItem(*output_index))?;
                let ChatOutputItem::FunctionCall(call) = item else {
                    return Err(ChatStreamAccumulatorError::ExpectedFunctionCall(
                        *output_index,
                    ));
                };
                call.arguments.push_str(delta);
            }
            StructuredStreamEvent::ResponseTerminal {
                status,
                usage,
                finish_reason,
                detail,
            } => {
                self.output.status = Some(*status);
                // Terminal events that omit usage/finish_reason must not erase
                // values accumulated from earlier stream events.
                if let Some(usage) = usage {
                    self.output.usage = Some(usage.clone());
                }
                if let Some(finish_reason) = finish_reason {
                    self.output.finish_reason = Some(*finish_reason);
                }
                self.terminal_detail.clone_from(detail);
                self.terminal_seen = true;
            }
        }
        Ok(())
    }

    fn ensure_item_open(&self, output_index: usize) -> Result<(), ChatStreamAccumulatorError> {
        if self.completed_items.contains(&output_index) {
            Err(ChatStreamAccumulatorError::EventAfterItemCompletion { output_index })
        } else {
            Ok(())
        }
    }
}

fn same_item_identity(left: &ChatOutputItem, right: &ChatOutputItem) -> bool {
    match (left, right) {
        (ChatOutputItem::Message(left), ChatOutputItem::Message(right)) => left.id == right.id,
        (ChatOutputItem::Reasoning(left), ChatOutputItem::Reasoning(right)) => left.id == right.id,
        (ChatOutputItem::FunctionCall(left), ChatOutputItem::FunctionCall(right)) => {
            left.item_id == right.item_id
                && left.call_id == right.call_id
                && left.name == right.name
        }
        (ChatOutputItem::Opaque(left), ChatOutputItem::Opaque(right)) => {
            left.original_type == right.original_type
        }
        _ => false,
    }
}

fn append_legacy_message_text(items: &mut Vec<ChatOutputItem>, delta: &str) {
    if let Some(ChatOutputItem::Message(message)) = items.last_mut()
        && let Some(ChatMessagePart::Text { text, .. }) = message.parts.last_mut()
    {
        text.push_str(delta);
        return;
    }
    items.push(ChatOutputItem::Message(super::ChatMessageItem {
        id: None,
        role: super::ChatRole::Assistant,
        phase: None,
        status: None,
        parts: vec![ChatMessagePart::Text {
            text: delta.to_string(),
            annotations: Vec::new(),
            extensions: Default::default(),
        }],
        extensions: Default::default(),
    }));
}

/// Whether a reasoning item is still open for text/signature deltas: it has
/// neither a signature nor opaque (redacted) content yet.
fn is_open_reasoning(reasoning: &super::ChatReasoningItem) -> bool {
    reasoning.signature.is_none() && reasoning.encrypted_content.is_none()
}

fn append_legacy_reasoning(items: &mut Vec<ChatOutputItem>, delta: &str) {
    if let Some(ChatOutputItem::Reasoning(reasoning)) = items.last_mut()
        && is_open_reasoning(reasoning)
        && let Some(part) = reasoning.content.last_mut()
    {
        part.text.push_str(delta);
        return;
    }
    items.push(ChatOutputItem::Reasoning(super::ChatReasoningItem {
        id: None,
        summary: Vec::new(),
        content: vec![ChatReasoningPart::text(delta)],
        encrypted_content: None,
        signature: None,
        status: None,
        extensions: Default::default(),
    }));
}

/// Attach a signature to the open reasoning item at the end of the output.
/// When there is none (an empty thinking block, e.g. omitted display), create
/// an empty reasoning item that carries the signature so blocks map one-to-one.
fn append_legacy_signature(items: &mut Vec<ChatOutputItem>, signature: &str) {
    if let Some(ChatOutputItem::Reasoning(reasoning)) = items.last_mut()
        && is_open_reasoning(reasoning)
    {
        reasoning.signature = Some(signature.to_string());
        return;
    }
    items.push(ChatOutputItem::Reasoning(super::ChatReasoningItem {
        id: None,
        summary: Vec::new(),
        content: Vec::new(),
        encrypted_content: None,
        signature: Some(signature.to_string()),
        status: None,
        extensions: Default::default(),
    }));
}

/// Marker extension identifying redacted Anthropic thinking blocks.
pub const ANTHROPIC_BLOCK_EXTENSION: &str = "anthropic_block";
/// Value of [`ANTHROPIC_BLOCK_EXTENSION`] for `redacted_thinking` blocks.
pub const ANTHROPIC_REDACTED_THINKING: &str = "redacted_thinking";

fn append_legacy_redacted(items: &mut Vec<ChatOutputItem>, data: &str) {
    let mut extensions = super::Extensions::new();
    extensions.insert(
        ANTHROPIC_BLOCK_EXTENSION.to_string(),
        serde_json::Value::String(ANTHROPIC_REDACTED_THINKING.to_string()),
    );
    items.push(ChatOutputItem::Reasoning(super::ChatReasoningItem {
        id: None,
        summary: Vec::new(),
        content: Vec::new(),
        encrypted_content: Some(data.to_string()),
        signature: None,
        status: None,
        extensions,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionCall, ToolCall};

    fn message(text: &str) -> ChatOutputItem {
        ChatOutputItem::Message(super::super::ChatMessageItem {
            id: Some("message_1".into()),
            role: super::super::ChatRole::Assistant,
            phase: None,
            status: None,
            parts: vec![ChatMessagePart::Text {
                text: text.into(),
                annotations: Vec::new(),
                extensions: Default::default(),
            }],
            extensions: Default::default(),
        })
    }

    #[test]
    fn structured_events_round_trip() {
        let events = vec![
            StructuredStreamEvent::ResponseMetadata {
                response_id: Some("resp_1".into()),
                status: Some(ChatOutputStatus::InProgress),
                usage: None,
                finish_reason: None,
                provenance: None,
            },
            StructuredStreamEvent::ItemStarted {
                output_index: 2,
                item: message(""),
            },
            StructuredStreamEvent::MessagePartDelta {
                output_index: 2,
                content_index: 0,
                delta: ChatMessagePartDelta::Text {
                    delta: "hello".into(),
                },
            },
            StructuredStreamEvent::ReasoningPartDelta {
                output_index: 0,
                part: ReasoningPartKind::Summary,
                part_index: 1,
                delta: "why".into(),
            },
            StructuredStreamEvent::FunctionArgumentsDelta {
                output_index: 1,
                delta: "{\"q\":".into(),
            },
            StructuredStreamEvent::ItemCompleted {
                output_index: 2,
                item: message("hello"),
            },
            StructuredStreamEvent::ResponseTerminal {
                status: ChatOutputStatus::Completed,
                usage: Some(Usage::default()),
                finish_reason: Some(FinishReason::Stop),
                detail: None,
            },
        ];
        for event in events {
            let chunk = StreamChunk::Structured(event.clone());
            let encoded = serde_json::to_string(&chunk).unwrap();
            let decoded: StreamChunk = serde_json::from_str(&encoded).unwrap();
            assert!(matches!(decoded, StreamChunk::Structured(actual) if actual == event));
        }
    }

    #[test]
    fn structured_mode_ignores_legacy_projections_and_usage() {
        let usage = Usage {
            input_tokens: 7,
            output_tokens: 3,
            ..Usage::default()
        };
        let mut accumulator = ChatStreamAccumulator::new();
        let chunks = [
            StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
                response_id: Some("resp_1".into()),
                status: Some(ChatOutputStatus::InProgress),
                usage: Some(usage.clone()),
                finish_reason: None,
                provenance: None,
            }),
            StreamChunk::Structured(StructuredStreamEvent::ItemStarted {
                output_index: 0,
                item: message(""),
            }),
            StreamChunk::Structured(StructuredStreamEvent::MessagePartDelta {
                output_index: 0,
                content_index: 0,
                delta: ChatMessagePartDelta::Text {
                    delta: "hello".into(),
                },
            }),
            StreamChunk::Text("hello".into()),
            StreamChunk::ToolUseComplete {
                index: 1,
                tool_call: ToolCall {
                    id: "duplicate".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "ignored".into(),
                        arguments: "{}".into(),
                    },
                },
                extensions: Default::default(),
            },
            StreamChunk::Usage(Usage {
                input_tokens: 99,
                output_tokens: 99,
                ..Usage::default()
            }),
        ];
        for chunk in &chunks {
            accumulator.push(chunk).unwrap();
        }
        let output = accumulator.output();
        assert!(accumulator.is_structured());
        assert_eq!(output.items, vec![message("hello")]);
        assert_eq!(output.text().as_deref(), Some("hello"));
        assert_eq!(output.usage, Some(usage));
        assert!(output.tool_calls().is_none());
    }

    #[test]
    fn legacy_mode_preserves_interleaved_sequence_and_merges_usage_once() {
        let mut accumulator = ChatStreamAccumulator::new();
        let chunks = [
            StreamChunk::Text("first".into()),
            StreamChunk::ToolUseComplete {
                index: 1,
                tool_call: ToolCall {
                    id: "call_1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "lookup".into(),
                        arguments: "{\"q\":1}".into(),
                    },
                },
                extensions: Default::default(),
            },
            StreamChunk::Text("second".into()),
            StreamChunk::Usage(Usage {
                input_tokens: 5,
                ..Usage::default()
            }),
            StreamChunk::Usage(Usage {
                output_tokens: 2,
                ..Usage::default()
            }),
        ];
        for chunk in &chunks {
            accumulator.push(chunk).unwrap();
        }
        let output = accumulator.output();
        assert_eq!(output.items.len(), 3);
        assert!(matches!(output.items[0], ChatOutputItem::Message(_)));
        assert!(matches!(output.items[1], ChatOutputItem::FunctionCall(_)));
        assert!(matches!(output.items[2], ChatOutputItem::Message(_)));
        assert_eq!(output.usage.unwrap().input_tokens, 5);
    }

    #[test]
    fn legacy_thinking_accumulates_as_content_not_summary() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::Thinking("full ".into()))
            .unwrap();
        accumulator
            .push(&StreamChunk::Thinking("thought".into()))
            .unwrap();
        let output = accumulator.output();
        let ChatOutputItem::Reasoning(reasoning) = &output.items[0] else {
            panic!("expected reasoning");
        };
        assert!(reasoning.summary.is_empty());
        assert_eq!(
            reasoning
                .content
                .iter()
                .map(|part| part.text.as_str())
                .collect::<Vec<_>>(),
            vec!["full thought"]
        );
        assert_eq!(output.thinking().as_deref(), Some("full thought"));
    }

    fn reasoning_items(output: &ChatOutput) -> Vec<&super::super::ChatReasoningItem> {
        output
            .items
            .iter()
            .filter_map(|item| match item {
                ChatOutputItem::Reasoning(r) => Some(r),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn legacy_consecutive_signed_blocks_stay_separate() {
        let mut accumulator = ChatStreamAccumulator::new();
        for chunk in [
            StreamChunk::Thinking("one".into()),
            StreamChunk::ThinkingSignature("S1".into()),
            StreamChunk::Text("progress".into()),
            StreamChunk::Thinking("two".into()),
            StreamChunk::ThinkingSignature("S2".into()),
        ] {
            accumulator.push(&chunk).unwrap();
        }
        let output = accumulator.output();
        let items = reasoning_items(&output);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].signature.as_deref(), Some("S1"));
        assert_eq!(items[0].content[0].text, "one");
        assert_eq!(items[1].signature.as_deref(), Some("S2"));
        assert_eq!(items[1].content[0].text, "two");

        // Back-to-back blocks with no text between them must not merge either.
        let mut accumulator = ChatStreamAccumulator::new();
        for chunk in [
            StreamChunk::Thinking("a".into()),
            StreamChunk::ThinkingSignature("A".into()),
            StreamChunk::Thinking("b".into()),
            StreamChunk::ThinkingSignature("B".into()),
        ] {
            accumulator.push(&chunk).unwrap();
        }
        let output = accumulator.output();
        let items = reasoning_items(&output);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].signature.as_deref(), Some("A"));
        assert_eq!(items[1].signature.as_deref(), Some("B"));
    }

    #[test]
    fn legacy_empty_signed_block_creates_empty_signed_item() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::ThinkingSignature("S".into()))
            .unwrap();
        let output = accumulator.output();
        let items = reasoning_items(&output);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].signature.as_deref(), Some("S"));
        assert!(items[0].visible_text().is_empty());
    }

    #[test]
    fn legacy_single_block_is_unchanged() {
        let mut accumulator = ChatStreamAccumulator::new();
        for chunk in [
            StreamChunk::Thinking("full ".into()),
            StreamChunk::Thinking("thought".into()),
            StreamChunk::ThinkingSignature("S".into()),
        ] {
            accumulator.push(&chunk).unwrap();
        }
        let output = accumulator.output();
        let items = reasoning_items(&output);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content[0].text, "full thought");
        assert_eq!(items[0].signature.as_deref(), Some("S"));
    }

    #[test]
    fn redacted_thinking_becomes_marked_encrypted_item() {
        let mut accumulator = ChatStreamAccumulator::new();
        for chunk in [
            StreamChunk::RedactedThinking("DATA".into()),
            StreamChunk::Thinking("next".into()),
            StreamChunk::ThinkingSignature("S".into()),
        ] {
            accumulator.push(&chunk).unwrap();
        }
        let output = accumulator.output();
        let items = reasoning_items(&output);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].encrypted_content.as_deref(), Some("DATA"));
        assert!(items[0].signature.is_none());
        assert!(items[0].visible_text().is_empty());
        assert_eq!(
            items[0]
                .extensions
                .get(super::ANTHROPIC_BLOCK_EXTENSION)
                .and_then(|v| v.as_str()),
            Some(super::ANTHROPIC_REDACTED_THINKING)
        );
        // Following thinking text must not be appended onto the redacted item.
        assert_eq!(items[1].content[0].text, "next");
        assert_eq!(items[1].signature.as_deref(), Some("S"));
    }

    #[test]
    fn redacted_thinking_chunk_debug_redacts_and_round_trips() {
        let chunk = StreamChunk::RedactedThinking("SECRET-DATA".into());
        assert!(!format!("{chunk:?}").contains("SECRET-DATA"));
        let json = serde_json::to_string(&chunk).unwrap();
        let back: StreamChunk = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StreamChunk::RedactedThinking(d) if d == "SECRET-DATA"));
    }

    #[test]
    fn completed_snapshot_replaces_deltas_and_repeated_snapshot_is_idempotent() {
        let started = message("");
        let completed = message("authoritative");
        let mut accumulator = ChatStreamAccumulator::new();
        let events = [
            StructuredStreamEvent::ResponseMetadata {
                response_id: None,
                status: Some(ChatOutputStatus::InProgress),
                usage: None,
                finish_reason: None,
                provenance: None,
            },
            StructuredStreamEvent::ItemStarted {
                output_index: 0,
                item: started,
            },
            StructuredStreamEvent::MessagePartDelta {
                output_index: 0,
                content_index: 0,
                delta: ChatMessagePartDelta::Text {
                    delta: "provisional".into(),
                },
            },
            StructuredStreamEvent::ItemCompleted {
                output_index: 0,
                item: completed.clone(),
            },
            StructuredStreamEvent::ItemCompleted {
                output_index: 0,
                item: completed.clone(),
            },
        ];
        for event in events {
            accumulator.push(&StreamChunk::Structured(event)).unwrap();
        }
        assert_eq!(accumulator.output().items, vec![completed]);
    }

    #[test]
    fn conflicting_completed_snapshot_identity_or_content_fails() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::Structured(metadata()))
            .unwrap();
        accumulator
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: message("first"),
                },
            ))
            .unwrap();

        assert_eq!(
            accumulator.push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: message("different"),
                }
            )),
            Err(ChatStreamAccumulatorError::ConflictingCompletion { output_index: 0 })
        );

        let mut different_identity = message("first");
        let ChatOutputItem::Message(message) = &mut different_identity else {
            unreachable!()
        };
        message.id = Some("message_2".into());
        assert_eq!(
            accumulator.push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 0,
                    item: different_identity,
                }
            )),
            Err(ChatStreamAccumulatorError::ConflictingItemIdentity { output_index: 0 })
        );
    }

    #[test]
    fn terminal_validation_distinguishes_eof_incomplete_failure_and_unfinished_items() {
        // Metadata alone never reaches a terminal, so finalization reports the
        // observable status rather than a success.
        let mut premature = ChatStreamAccumulator::new();
        premature
            .push(&StreamChunk::Structured(metadata()))
            .unwrap();
        match premature.finish() {
            ChatStreamFinish::Failed { error, .. } => assert_eq!(
                error,
                ChatStreamAccumulatorError::InvalidTerminalStatus(ChatOutputStatus::InProgress)
            ),
            other => panic!("expected failed outcome, got {other:?}"),
        }

        // No events at all is a premature EOF.
        let empty = ChatStreamAccumulator::new();
        match empty.finish() {
            ChatStreamFinish::Failed { error, output } => {
                assert_eq!(error, ChatStreamAccumulatorError::PrematureEof);
                assert!(output.items.is_empty());
            }
            other => panic!("expected failed outcome, got {other:?}"),
        }

        let mut unfinished = ChatStreamAccumulator::new();
        unfinished
            .push(&StreamChunk::Structured(metadata()))
            .unwrap();
        unfinished
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemStarted {
                    output_index: 4,
                    item: message("partial"),
                },
            ))
            .unwrap();
        unfinished
            .push(&StreamChunk::Structured(terminal(
                ChatOutputStatus::Completed,
                None,
            )))
            .unwrap();
        match unfinished.finish() {
            ChatStreamFinish::Failed { error, output } => {
                assert_eq!(error, ChatStreamAccumulatorError::IncompleteItems(vec![4]));
                // Partial items remain inspectable in the failed outcome.
                assert_eq!(output.items, vec![message("partial")]);
            }
            other => panic!("expected failed outcome, got {other:?}"),
        }

        for (status, expected) in [
            (ChatOutputStatus::Incomplete, "max_output_tokens"),
            (ChatOutputStatus::Failed, "provider_error"),
        ] {
            let mut accumulator = ChatStreamAccumulator::new();
            accumulator
                .push(&StreamChunk::Structured(metadata()))
                .unwrap();
            accumulator
                .push(&StreamChunk::Structured(terminal(status, Some(expected))))
                .unwrap();
            match accumulator.finish() {
                ChatStreamFinish::Incomplete { detail, .. } => {
                    assert_eq!(detail.as_deref(), Some(expected));
                }
                ChatStreamFinish::Failed { error, .. } => {
                    assert_eq!(
                        error,
                        ChatStreamAccumulatorError::FailedResponse(Some(expected.to_string()))
                    );
                }
                other => panic!("expected non-success outcome, got {other:?}"),
            }
        }
    }

    #[test]
    fn legacy_terminal_rejects_unfinished_tool_call() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::ToolUseStart {
                index: 3,
                id: "call_3".into(),
                name: "lookup".into(),
            })
            .unwrap();
        accumulator
            .push(&StreamChunk::Done {
                finish_reason: FinishReason::ToolCalls,
            })
            .unwrap();
        match accumulator.finish() {
            ChatStreamFinish::Failed { error, .. } => assert_eq!(
                error,
                ChatStreamAccumulatorError::IncompleteToolCalls(vec![3])
            ),
            other => panic!("expected failed outcome, got {other:?}"),
        }
    }

    #[test]
    fn legacy_length_stop_with_unfinished_tool_call_is_incomplete() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::ToolUseStart {
                index: 0,
                id: "call_0".into(),
                name: "write_file".into(),
            })
            .unwrap();
        accumulator
            .push(&StreamChunk::Done {
                finish_reason: FinishReason::Length,
            })
            .unwrap();
        match accumulator.finish() {
            ChatStreamFinish::Incomplete { output, detail } => {
                assert_eq!(detail.as_deref(), Some(LENGTH_CUTOFF_DETAIL));
                assert_eq!(output.finish_reason, Some(FinishReason::Length));
                assert_eq!(output.status, Some(ChatOutputStatus::Incomplete));
            }
            other => panic!("expected incomplete outcome, got {other:?}"),
        }
    }

    #[test]
    fn legacy_length_stop_without_open_calls_still_completes() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::Text("partial".into()))
            .unwrap();
        accumulator
            .push(&StreamChunk::Done {
                finish_reason: FinishReason::Length,
            })
            .unwrap();
        assert!(accumulator.finish().is_completed());
    }

    fn legacy_tool_complete(index: usize, id: &str) -> StreamChunk {
        StreamChunk::ToolUseComplete {
            index,
            tool_call: ToolCall {
                id: id.into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "lookup".into(),
                    arguments: "{}".into(),
                },
            },
            extensions: Default::default(),
        }
    }

    #[test]
    fn legacy_orphan_argument_delta_does_not_fail_response() {
        let mut accumulator = ChatStreamAccumulator::new();
        for chunk in [
            StreamChunk::ToolUseStart {
                index: 0,
                id: "call_0".into(),
                name: "lookup".into(),
            },
            StreamChunk::ToolUseInputDelta {
                index: 1,
                partial_json: "{\"a\":".into(),
            },
            StreamChunk::ToolUseInputDelta {
                index: 1,
                partial_json: "1}".into(),
            },
            legacy_tool_complete(0, "call_0"),
            // A late delta for a completed call is not an orphan.
            StreamChunk::ToolUseInputDelta {
                index: 0,
                partial_json: "x".into(),
            },
            StreamChunk::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ] {
            accumulator.push(&chunk).unwrap();
        }
        // Index 1 is reported once; the late delta for 0 is not reported.
        assert_eq!(
            accumulator.warned_orphan_calls,
            BTreeSet::from([1]),
            "one orphan warning, for index 1 only"
        );
        match accumulator.finish() {
            ChatStreamFinish::Completed(output) => {
                let calls: Vec<_> = output
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        ChatOutputItem::FunctionCall(call) => Some(call.arguments.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(calls, ["{}"], "completed call unchanged");
            }
            other => panic!("expected completed outcome, got {other:?}"),
        }
    }

    #[test]
    fn legacy_started_call_with_deltas_but_no_completion_still_fails() {
        let mut accumulator = ChatStreamAccumulator::new();
        for chunk in [
            StreamChunk::ToolUseStart {
                index: 1,
                id: "call_1".into(),
                name: "lookup".into(),
            },
            StreamChunk::ToolUseInputDelta {
                index: 1,
                partial_json: "{\"a\":1}".into(),
            },
            StreamChunk::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ] {
            accumulator.push(&chunk).unwrap();
        }
        match accumulator.finish() {
            ChatStreamFinish::Failed { error, .. } => assert_eq!(
                error,
                ChatStreamAccumulatorError::IncompleteToolCalls(vec![1])
            ),
            other => panic!("expected failed outcome, got {other:?}"),
        }
    }

    #[test]
    fn reset_attempt_discards_partial_items_and_allows_clean_retry() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::Structured(metadata()))
            .unwrap();
        accumulator
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemStarted {
                    output_index: 0,
                    item: message("old partial"),
                },
            ))
            .unwrap();

        accumulator.reset_attempt();
        accumulator
            .push(&StreamChunk::Structured(metadata()))
            .unwrap();
        accumulator
            .push(&StreamChunk::Structured(
                StructuredStreamEvent::ItemCompleted {
                    output_index: 1,
                    item: message("retry output"),
                },
            ))
            .unwrap();
        accumulator
            .push(&StreamChunk::Structured(terminal(
                ChatOutputStatus::Completed,
                None,
            )))
            .unwrap();

        let output = accumulator.finish_success().unwrap();
        assert_eq!(output.items, vec![message("retry output")]);
        assert_eq!(output.text().as_deref(), Some("retry output"));
    }

    fn metadata() -> StructuredStreamEvent {
        StructuredStreamEvent::ResponseMetadata {
            response_id: None,
            status: Some(ChatOutputStatus::InProgress),
            usage: None,
            finish_reason: None,
            provenance: None,
        }
    }

    fn terminal(status: ChatOutputStatus, detail: Option<&str>) -> StructuredStreamEvent {
        StructuredStreamEvent::ResponseTerminal {
            status,
            usage: None,
            finish_reason: (status == ChatOutputStatus::Completed).then_some(FinishReason::Stop),
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn metadata_after_legacy_output_is_rejected() {
        let mut accumulator = ChatStreamAccumulator::new();
        accumulator
            .push(&StreamChunk::Text("legacy".into()))
            .unwrap();
        assert_eq!(
            accumulator.push(&StreamChunk::Structured(
                StructuredStreamEvent::ResponseMetadata {
                    response_id: None,
                    status: None,
                    usage: None,
                    finish_reason: None,
                    provenance: None,
                }
            )),
            Err(ChatStreamAccumulatorError::LateStructuredMetadata)
        );
    }
}
