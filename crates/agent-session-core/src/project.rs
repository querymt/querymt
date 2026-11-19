//! Journal event to ACP update projection.
//!
//! Snapshot and history-page events become the same ACP updates the agent
//! sends live, so one set of handlers builds the view from either source.

use crate::decode::Event;
use querymt_agent_protocol_types::events::AgentEventKind;
use querymt_agent_protocol_types::protocol::{
    CompactionId, CompactionStatus, CompactionSummaryChunk, CompactionUpdate, Content,
    ContentBlock, ContentChunk, MessageId, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};

/// Tool kind by tool name, as the agent reports it.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "search_text" | "mdq" => ToolKind::Search,
        "write_file" | "edit" | "multiedit" | "accept_delegation_result" => ToolKind::Edit,
        "delete_file" => ToolKind::Delete,
        "shell" => ToolKind::Execute,
        "web_fetch" | "browse" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn text(content: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(content.to_owned()))
}

fn thought(content: &str, message_id: &str, part_id: Option<&str>) -> SessionUpdate {
    let chunk = ContentChunk::new(text(content)).message_id(MessageId::from(message_id.to_owned()));
    let chunk = match part_id.filter(|id| !id.is_empty()) {
        Some(part) => chunk.meta(serde_json::Map::from_iter([(
            "querymt".to_owned(),
            serde_json::json!({ "reasoning_part_id": part }),
        )])),
        None => chunk,
    };
    SessionUpdate::AgentThoughtChunk(chunk)
}

/// Compaction id: explicit when present, else derived from the event position.
fn compaction_id(explicit: Option<&String>, seq: i64) -> CompactionId {
    match explicit.filter(|id| !id.is_empty()) {
        Some(id) => CompactionId::from(id.clone()),
        None => CompactionId::from(format!("querymt-compaction-legacy-{seq}")),
    }
}

/// The updates one journal event stands for. Events that carry no
/// conversation content project to nothing.
pub(crate) fn event_updates(event: &Event) -> Vec<SessionUpdate> {
    match &event.kind {
        AgentEventKind::PromptReceived {
            content,
            message_id,
        } => {
            vec![SessionUpdate::UserMessageChunk(
                ContentChunk::new(text(content))
                    .message_id(message_id.clone().map(MessageId::from)),
            )]
        }
        AgentEventKind::AssistantMessageStored {
            content,
            message_id,
            reasoning_parts,
            ..
        } => {
            let mut updates = Vec::new();
            if let Some(id) = message_id {
                for part in reasoning_parts.iter().filter(|p| !p.text.is_empty()) {
                    updates.push(thought(&part.text, id, Some(&part.id)));
                }
            }
            if !content.is_empty() {
                updates.push(SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(text(content))
                        .message_id(message_id.clone().map(MessageId::from)),
                ));
            }
            updates
        }
        AgentEventKind::AssistantThinkingDelta {
            content,
            message_id,
            part_id,
        } if !content.is_empty() => vec![thought(content, message_id, part_id.as_deref())],
        AgentEventKind::ToolCallStart {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
            vec![SessionUpdate::ToolCall(
                ToolCall::new(
                    ToolCallId::from(tool_call_id.clone()),
                    format!("Run {tool_name}"),
                )
                .kind(tool_kind(tool_name))
                .status(ToolCallStatus::InProgress)
                .raw_input(args),
            )]
        }
        AgentEventKind::ToolCallEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            let status = if *is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::from(tool_call_id.clone()),
                ToolCallUpdateFields::new()
                    .kind(tool_kind(tool_name))
                    .status(status)
                    .title(format!("Run {tool_name}"))
                    .content(vec![ToolCallContent::Content(Content::new(text(result)))])
                    .raw_output(serde_json::from_str::<serde_json::Value>(result).ok()),
            ))]
        }
        AgentEventKind::CompactionStart {
            compaction_id: id, ..
        } => {
            vec![SessionUpdate::CompactionUpdate(CompactionUpdate::new(
                compaction_id(id.as_ref(), event.seq),
                CompactionStatus::InProgress,
            ))]
        }
        AgentEventKind::CompactionSummaryChunk {
            compaction_id: id,
            content,
        } => vec![SessionUpdate::CompactionSummaryChunk(
            CompactionSummaryChunk::new(CompactionId::from(id.clone()), text(content)),
        )],
        AgentEventKind::CompactionEnd {
            summary,
            compaction_id: id,
            ..
        } => {
            let mut update = CompactionUpdate::new(
                compaction_id(id.as_ref(), event.seq),
                CompactionStatus::Completed,
            );
            if !summary.trim().is_empty() {
                update = update.summary(Some(vec![text(summary)]));
            }
            vec![SessionUpdate::CompactionUpdate(update)]
        }
        AgentEventKind::CompactionFailed {
            compaction_id: id,
            reason,
            cancelled,
        } => {
            let status = if *cancelled {
                CompactionStatus::Cancelled
            } else {
                CompactionStatus::Failed
            };
            vec![SessionUpdate::CompactionUpdate(
                CompactionUpdate::new(CompactionId::from(id.clone()), status).error(reason.clone()),
            )]
        }
        _ => Vec::new(),
    }
}
