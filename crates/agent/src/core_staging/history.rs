//! Effective-history filtering: the messages that follow the last compaction.

use crate::model::{AgentMessage, MessagePart};
use querymt::chat::ChatRole;

/// Filter messages to return only the "effective" history after the last compaction.
///
/// This function finds the most recent compaction message and returns only
/// messages from that point forward (including the compaction itself).
///
/// # Arguments
///
/// * `messages` - The full conversation history
///
/// # Returns
///
/// A subset of messages starting from (and including) the last compaction,
/// or all messages if no compaction exists.
pub fn filter_to_effective_history(messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    // Find the index of the last compaction request (user message).
    // This anchors on the user side of the compaction pair so that the
    // effective history starts with the user→assistant compaction exchange.
    let last_compaction_idx = messages.iter().rposition(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::CompactionRequest { .. }))
    });

    let filtered: Vec<AgentMessage> = match last_compaction_idx {
        Some(idx) => messages.into_iter().skip(idx).collect(),
        None => messages, // No compaction found, return all messages
    };

    // Filter out messages that only contain snapshot metadata parts
    // These are for undo/redo tracking and should not be sent to the LLM
    // Keeping them creates empty messages that break tool_use -> tool_result sequencing
    let without_snapshots = filtered
        .into_iter()
        .filter(|m| {
            m.parts.iter().any(|p| {
                !matches!(
                    p,
                    MessagePart::TurnSnapshotStart { .. } | MessagePart::TurnSnapshotPatch { .. }
                )
            })
        })
        .collect::<Vec<_>>();

    // Safety net: merge consecutive User messages that each contain a
    // tool_result part.  Legacy sessions stored one message per tool result
    // which violates the LLM API contract that all tool_results for a batch
    // of tool_use calls appear in a single user message.
    merge_consecutive_tool_result_messages(without_snapshots)
}

/// Merge runs of consecutive `User` messages where **every** message in the
/// run contains at least one `ToolResult` part.  The parts from subsequent
/// messages are folded into the first message of the run.
///
/// This repairs data written by older versions of `store_all_tool_results`
/// which created one message per tool result instead of batching them.
pub fn merge_consecutive_tool_result_messages(messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
    if messages.len() < 2 {
        return messages;
    }

    fn is_user_tool_result(m: &AgentMessage) -> bool {
        m.role == ChatRole::User
            && m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }))
    }

    let mut result: Vec<AgentMessage> = Vec::with_capacity(messages.len());

    for msg in messages {
        let dominated_by_tool_result = is_user_tool_result(&msg);
        let should_merge =
            dominated_by_tool_result && result.last().is_some_and(is_user_tool_result);

        if should_merge {
            // Safe: we just checked last() is Some.
            let prev = result.last_mut().unwrap();
            prev.parts.extend(msg.parts);
        } else {
            result.push(msg);
        }
    }

    result
}
