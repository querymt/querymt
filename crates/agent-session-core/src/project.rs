//! Journal event to ACP update projection.
//!
//! Snapshot and history-page events become the same ACP updates the agent
//! sends live, so one set of handlers builds the view from either source.

use crate::decode::Event;
use querymt_agent_protocol_types::events::AgentEventKind;
use querymt_agent_protocol_types::protocol::{
    CompactionId, CompactionStatus, CompactionSummaryChunk, CompactionUpdate, ContentBlock, PlanId,
    PlanRemoved, SessionUpdate, TextContent,
};
use querymt_agent_protocol_types::replay::{
    CompactionIds, QUERYMT_TODO_PLAN_ID, is_todo_write_tool, replayable_updates,
    stamp_compaction_tokens, stored_summary_updates, todo_entries_from_arguments, todo_plan_update,
};

/// The updates one journal event stands for. Events that carry no
/// conversation content project to nothing.
///
/// Conversation, tool, plan and workspace content comes from the projection
/// the agent's own replay uses (`querymt_agent_protocol_types::replay`), so a
/// loaded session and a replayed one cannot differ. `ids` carries compaction
/// identity across the events of one journal, in order.
pub(crate) fn event_updates(event: &Event, ids: &mut CompactionIds) -> Vec<SessionUpdate> {
    let updates = event_updates_inner(event, ids);
    // The context this event measured counts for the compactions after it.
    ids.observe(&event.kind);
    updates
}

fn event_updates_inner(event: &Event, ids: &mut CompactionIds) -> Vec<SessionUpdate> {
    let text = |content: &str| ContentBlock::Text(TextContent::new(content.to_owned()));
    match &event.kind {
        AgentEventKind::AssistantMessageStored {
            content,
            message_id,
            reasoning_parts,
            ..
        } => stored_summary_updates(content, message_id.as_deref(), reasoning_parts),
        AgentEventKind::CompactionStart {
            compaction_id: id,
            token_estimate,
        } => {
            let compaction_id = ids.start(id.as_deref(), event.seq);
            let before = ids.start_tokens(&compaction_id, *token_estimate);
            let mut update = SessionUpdate::CompactionUpdate(CompactionUpdate::new(
                compaction_id,
                CompactionStatus::InProgress,
            ));
            stamp_compaction_tokens(&mut update, Some(before), None);
            vec![update]
        }
        AgentEventKind::CompactionSummaryChunk {
            compaction_id: id,
            content,
        } => vec![SessionUpdate::CompactionSummaryChunk(
            CompactionSummaryChunk::new(id.clone(), text(content)),
        )],
        AgentEventKind::CompactionEnd {
            summary,
            compaction_id: id,
            context_tokens,
            ..
        } => {
            let compaction_id = ids.end(id.as_deref(), event.seq);
            let before = ids.take_tokens_before(&compaction_id);
            let mut update = CompactionUpdate::new(compaction_id, CompactionStatus::Completed);
            if !summary.trim().is_empty() {
                update = update.summary(Some(vec![text(summary)]));
            }
            let mut update = SessionUpdate::CompactionUpdate(update);
            stamp_compaction_tokens(&mut update, before, *context_tokens);
            vec![update]
        }
        AgentEventKind::CompactionFailed {
            compaction_id: id,
            reason,
            cancelled,
        } => {
            ids.failed();
            let before = ids.take_tokens_before(&CompactionId::from(id.clone()));
            let (status, error) = if *cancelled {
                (CompactionStatus::Cancelled, None)
            } else {
                (CompactionStatus::Failed, Some(reason.clone()))
            };
            let update = CompactionUpdate::new(id.clone(), status);
            let mut update = SessionUpdate::CompactionUpdate(match error {
                Some(error) => update.error(error),
                None => update,
            });
            stamp_compaction_tokens(&mut update, before, None);
            vec![update]
        }
        // The reducer always supports plan operations, so a todo list is the
        // `plan_update` under the shared plan id, as the agent sends it live.
        AgentEventKind::ToolCallStart {
            tool_name,
            arguments,
            ..
        } if is_todo_write_tool(tool_name) => todo_entries_from_arguments(arguments)
            .map(|entries| {
                // An empty list dismisses the plan, as it does live.
                if entries.is_empty() {
                    vec![SessionUpdate::PlanRemoved(PlanRemoved::new(PlanId::from(
                        QUERYMT_TODO_PLAN_ID.to_string(),
                    )))]
                } else {
                    vec![todo_plan_update(entries)]
                }
            })
            .unwrap_or_default(),
        kind => replayable_updates(kind),
    }
}
