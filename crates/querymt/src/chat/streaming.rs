use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    ChatFunctionCallItem, ChatMessagePart, ChatOutput, ChatOutputItem, ChatOutputProvenance,
    ChatOutputRepresentation, ChatOutputStatus, ChatReasoningPart, FinishReason, StreamChunk,
};
use crate::Usage;

/// Item-aware stream events. Providers emit metadata before semantic events to declare
/// structured mode; legacy chunks may still accompany these events as UI projections.
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
    MessagePartDelta {
        output_index: usize,
        content_index: usize,
        delta: ChatMessagePartDelta,
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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatStreamAccumulatorError {
    #[error("structured response metadata must precede semantic output")]
    LateStructuredMetadata,
    #[error("structured item delta references missing output index {0}")]
    MissingItem(usize),
    #[error("structured message delta references non-message output index {0}")]
    ExpectedMessage(usize),
    #[error(
        "structured message delta references missing content index {content_index} at output index {output_index}"
    )]
    MissingMessagePart {
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
    #[error(
        "structured reasoning delta references missing part index {part_index} at output index {output_index}"
    )]
    MissingReasoningPart {
        output_index: usize,
        part_index: usize,
    },
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
    completed_items: BTreeMap<usize, ChatOutputItem>,
    pending_legacy_calls: BTreeSet<usize>,
    terminal_detail: Option<String>,
    terminal_seen: bool,
}

impl Default for ChatStreamAccumulator {
    fn default() -> Self {
        Self {
            mode: AccumulationMode::Undecided,
            output: ChatOutput::default(),
            structured_items: BTreeMap::new(),
            completed_items: BTreeMap::new(),
            pending_legacy_calls: BTreeSet::new(),
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
                self.output.representation = ChatOutputRepresentation::Structured;
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
            StreamChunk::ToolUseComplete { index, tool_call } => {
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
                        extensions: Default::default(),
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
                self.pending_legacy_calls.insert(*index);
            }
            StreamChunk::ToolUseInputDelta { index, .. } => {
                self.select_legacy();
                self.pending_legacy_calls.insert(*index);
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

    /// Validate semantic completion and return the attempt output.
    pub fn finish(&self) -> Result<ChatOutput, ChatStreamAccumulatorError> {
        if !self.terminal_seen {
            return Err(ChatStreamAccumulatorError::PrematureEof);
        }
        if self.mode == AccumulationMode::Legacy && !self.pending_legacy_calls.is_empty() {
            return Err(ChatStreamAccumulatorError::IncompleteToolCalls(
                self.pending_legacy_calls.iter().copied().collect(),
            ));
        }
        if self.mode == AccumulationMode::Structured {
            let unfinished: Vec<usize> = self
                .structured_items
                .keys()
                .filter(|index| !self.completed_items.contains_key(index))
                .copied()
                .collect();
            if self.output.status == Some(ChatOutputStatus::Completed) && !unfinished.is_empty() {
                return Err(ChatStreamAccumulatorError::IncompleteItems(unfinished));
            }
        }
        match self.output.status {
            Some(ChatOutputStatus::Completed) => Ok(self.output()),
            Some(ChatOutputStatus::Incomplete) => Err(
                ChatStreamAccumulatorError::IncompleteResponse(self.terminal_detail.clone()),
            ),
            Some(ChatOutputStatus::Failed) => Err(ChatStreamAccumulatorError::FailedResponse(
                self.terminal_detail.clone(),
            )),
            Some(status) => Err(ChatStreamAccumulatorError::InvalidTerminalStatus(status)),
            None => Err(ChatStreamAccumulatorError::PrematureEof),
        }
    }

    /// Start an isolated physical request attempt, discarding all prior partial state.
    pub fn reset_attempt(&mut self) {
        *self = Self::default();
    }

    fn select_legacy(&mut self) {
        if self.mode == AccumulationMode::Undecided {
            self.mode = AccumulationMode::Legacy;
            self.output.representation = ChatOutputRepresentation::LegacyProjection;
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
                if let Some(completed) = self.completed_items.get(output_index) {
                    if completed != item {
                        return Err(ChatStreamAccumulatorError::ConflictingCompletion {
                            output_index: *output_index,
                        });
                    }
                    return Ok(());
                }
                self.structured_items.insert(*output_index, item.clone());
                self.completed_items.insert(*output_index, item.clone());
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
                let part = message.parts.get_mut(*content_index).ok_or(
                    ChatStreamAccumulatorError::MissingMessagePart {
                        output_index: *output_index,
                        content_index: *content_index,
                    },
                )?;
                match (part, delta) {
                    (ChatMessagePart::Text { text, .. }, ChatMessagePartDelta::Text { delta }) => {
                        text.push_str(delta);
                    }
                    (
                        ChatMessagePart::Refusal { refusal, .. },
                        ChatMessagePartDelta::Refusal { delta },
                    ) => refusal.push_str(delta),
                    _ => {
                        return Err(ChatStreamAccumulatorError::MessagePartTypeMismatch {
                            output_index: *output_index,
                            content_index: *content_index,
                        });
                    }
                }
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
                let part = parts.get_mut(*part_index).ok_or(
                    ChatStreamAccumulatorError::MissingReasoningPart {
                        output_index: *output_index,
                        part_index: *part_index,
                    },
                )?;
                part.text.push_str(delta);
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
                self.output.usage.clone_from(usage);
                self.output.finish_reason = *finish_reason;
                self.terminal_detail.clone_from(detail);
                self.terminal_seen = true;
            }
        }
        Ok(())
    }

    fn ensure_item_open(&self, output_index: usize) -> Result<(), ChatStreamAccumulatorError> {
        if self.completed_items.contains_key(&output_index) {
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

fn append_legacy_reasoning(items: &mut Vec<ChatOutputItem>, delta: &str) {
    if let Some(ChatOutputItem::Reasoning(reasoning)) = items.last_mut()
        && let Some(part) = reasoning.summary.last_mut()
    {
        part.text.push_str(delta);
        return;
    }
    items.push(ChatOutputItem::Reasoning(super::ChatReasoningItem {
        id: None,
        summary: vec![ChatReasoningPart::text(delta)],
        content: Vec::new(),
        encrypted_content: None,
        signature: None,
        status: None,
        extensions: Default::default(),
    }));
}

fn append_legacy_signature(items: &mut Vec<ChatOutputItem>, signature: &str) {
    if let Some(reasoning) = items.iter_mut().rev().find_map(|item| match item {
        ChatOutputItem::Reasoning(reasoning) => Some(reasoning),
        _ => None,
    }) {
        reasoning.signature = Some(signature.to_string());
    }
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
        let mut premature = ChatStreamAccumulator::new();
        premature
            .push(&StreamChunk::Structured(metadata()))
            .unwrap();
        assert_eq!(
            premature.finish(),
            Err(ChatStreamAccumulatorError::PrematureEof)
        );

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
        assert_eq!(
            unfinished.finish(),
            Err(ChatStreamAccumulatorError::IncompleteItems(vec![4]))
        );

        for (status, expected) in [
            (
                ChatOutputStatus::Incomplete,
                ChatStreamAccumulatorError::IncompleteResponse(Some("max_output_tokens".into())),
            ),
            (
                ChatOutputStatus::Failed,
                ChatStreamAccumulatorError::FailedResponse(Some("provider_error".into())),
            ),
        ] {
            let mut accumulator = ChatStreamAccumulator::new();
            accumulator
                .push(&StreamChunk::Structured(metadata()))
                .unwrap();
            accumulator
                .push(&StreamChunk::Structured(terminal(
                    status,
                    Some(match status {
                        ChatOutputStatus::Incomplete => "max_output_tokens",
                        _ => "provider_error",
                    }),
                )))
                .unwrap();
            assert_eq!(accumulator.finish(), Err(expected));
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
        assert_eq!(
            accumulator.finish(),
            Err(ChatStreamAccumulatorError::IncompleteToolCalls(vec![3]))
        );
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

        let output = accumulator.finish().unwrap();
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
