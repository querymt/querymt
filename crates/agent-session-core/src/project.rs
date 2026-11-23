//! Journal event to ACP update projection.
//!
//! Snapshot and history-page events become the same ACP updates the agent
//! sends live, so one set of handlers builds the view from either source.

use crate::decode::Event;
use querymt_agent_protocol_types::events::AgentEventKind;
use querymt_agent_protocol_types::protocol::{
    CompactionStatus, CompactionSummaryChunk, CompactionUpdate, ContentBlock, SessionUpdate,
    TextContent,
};
use querymt_agent_protocol_types::replay::{
    CompactionIds, replayable_updates, stored_summary_updates,
};

/// The updates one journal event stands for. Events that carry no
/// conversation content project to nothing.
///
/// Conversation, tool, plan and workspace content comes from the projection
/// the agent's own replay uses (`querymt_agent_protocol_types::replay`), so a
/// loaded session and a replayed one cannot differ. `ids` carries compaction
/// identity across the events of one journal, in order.
pub(crate) fn event_updates(event: &Event, ids: &mut CompactionIds) -> Vec<SessionUpdate> {
    let text = |content: &str| ContentBlock::Text(TextContent::new(content.to_owned()));
    match &event.kind {
        AgentEventKind::AssistantMessageStored {
            content,
            message_id,
            reasoning_parts,
            ..
        } => stored_summary_updates(content, message_id.as_deref(), reasoning_parts),
        AgentEventKind::CompactionStart {
            compaction_id: id, ..
        } => vec![SessionUpdate::CompactionUpdate(CompactionUpdate::new(
            ids.start(id.as_deref(), event.seq),
            CompactionStatus::InProgress,
        ))],
        AgentEventKind::CompactionSummaryChunk {
            compaction_id: id,
            content,
        } => vec![SessionUpdate::CompactionSummaryChunk(
            CompactionSummaryChunk::new(id.clone(), text(content)),
        )],
        AgentEventKind::CompactionEnd {
            summary,
            compaction_id: id,
            ..
        } => {
            let mut update = CompactionUpdate::new(
                ids.end(id.as_deref(), event.seq),
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
            ids.failed();
            let (status, error) = if *cancelled {
                (CompactionStatus::Cancelled, None)
            } else {
                (CompactionStatus::Failed, Some(reason.clone()))
            };
            let update = CompactionUpdate::new(id.clone(), status);
            vec![SessionUpdate::CompactionUpdate(match error {
                Some(error) => update.error(error),
                None => update,
            })]
        }
        kind => replayable_updates(kind),
    }
}
