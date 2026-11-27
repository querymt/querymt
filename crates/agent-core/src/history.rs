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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal message builders (the compaction tests keep their own fixture).
    struct MessageFixture;

    impl MessageFixture {
        fn base(
            id: &str,
            session_id: &str,
            role: ChatRole,
            parts: Vec<MessagePart>,
        ) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role,
                parts,
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        fn text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
            Self::base(
                id,
                session_id,
                role,
                vec![MessagePart::Text {
                    content: text.to_string(),
                }],
            )
        }

        fn assistant_message(id: &str, session_id: &str, text: &str) -> AgentMessage {
            Self::text_message(id, session_id, ChatRole::Assistant, text)
        }

        fn user_message(id: &str, session_id: &str, text: &str) -> AgentMessage {
            Self::text_message(id, session_id, ChatRole::User, text)
        }

        fn compaction_request_message(id: &str, session_id: &str) -> AgentMessage {
            Self::base(
                id,
                session_id,
                ChatRole::User,
                vec![MessagePart::CompactionRequest {
                    original_token_count: 1000,
                }],
            )
        }

        fn compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
            Self::base(
                id,
                session_id,
                ChatRole::Assistant,
                vec![MessagePart::Compaction {
                    summary: summary.to_string(),
                    original_token_count: 1000,
                }],
            )
        }
    }

    fn make_text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
        MessageFixture::text_message(id, session_id, role, text)
    }

    fn make_compaction_request_message(id: &str, session_id: &str) -> AgentMessage {
        MessageFixture::compaction_request_message(id, session_id)
    }

    fn make_compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
        MessageFixture::compaction_message(id, session_id, summary)
    }

    /// Helper: a User message with a single tool_result part.
    fn make_user_tool_result(id: &str, session_id: &str, call_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::ToolResult {
                call_id: call_id.to_string(),
                content: vec![querymt::chat::ToolResultPart::text("output")],
                is_error: false,
                tool_name: Some("tool".to_string()),
                tool_arguments: None,
                compacted_at: None,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    /// Helper: an Assistant message with tool_use parts.
    fn make_assistant_tool_use(id: &str, session_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::ToolUse(querymt::ToolCall {
                id: "tu-1".to_string(),
                call_type: "function".to_string(),
                function: querymt::FunctionCall {
                    name: "tool".to_string(),
                    arguments: "{}".to_string(),
                },
            })],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    #[test]
    fn test_filter_returns_all_if_no_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Hello"),
            make_text_message("2", "s1", ChatRole::Assistant, "Hi there"),
            make_text_message("3", "s1", ChatRole::User, "How are you?"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].id, "1");
    }

    #[test]
    fn test_filter_returns_after_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Old message"),
            make_text_message("2", "s1", ChatRole::Assistant, "Old response"),
            make_compaction_request_message("3", "s1"),
            make_compaction_message("4", "s1", "Summary of previous conversation"),
            make_text_message("5", "s1", ChatRole::User, "New message"),
            make_text_message("6", "s1", ChatRole::Assistant, "New response"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 4);
        assert_eq!(filtered[0].id, "3"); // CompactionRequest (user)
        assert_eq!(filtered[1].id, "4"); // Compaction (assistant)
        assert_eq!(filtered[2].id, "5");
        assert_eq!(filtered[3].id, "6");
    }

    #[test]
    fn test_filter_uses_last_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Very old"),
            make_compaction_request_message("2", "s1"),
            make_compaction_message("3", "s1", "First summary"),
            make_text_message("4", "s1", ChatRole::User, "Medium old"),
            make_compaction_request_message("5", "s1"),
            make_compaction_message("6", "s1", "Second summary"),
            make_text_message("7", "s1", ChatRole::User, "Recent"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].id, "5"); // Second CompactionRequest
        assert_eq!(filtered[1].id, "6"); // Second Compaction
        assert_eq!(filtered[2].id, "7");
    }

    #[test]
    fn test_filter_drops_compacted_structured_exchange_without_dangling_results() {
        use querymt::chat::{
            ChatFunctionCallItem, ChatMessageItem, ChatMessagePart, ChatOutput, ChatOutputItem,
        };

        fn structured_assistant(id: &str, session_id: &str, call_id: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Output {
                    output: ChatOutput {
                        items: vec![
                            ChatOutputItem::Message(ChatMessageItem {
                                id: None,
                                role: ChatRole::Assistant,
                                phase: None,
                                status: None,
                                parts: vec![ChatMessagePart::Text {
                                    text: "calling tool".into(),
                                    annotations: Vec::new(),
                                    extensions: Default::default(),
                                }],
                                extensions: Default::default(),
                            }),
                            ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                                item_id: Some(format!("item_{call_id}")),
                                call_id: call_id.to_string(),
                                name: "shell".into(),
                                arguments: "{}".into(),
                                status: None,
                                extensions: Default::default(),
                            }),
                        ],
                        ..ChatOutput::default()
                    },
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        fn tool_result_user(id: &str, session_id: &str, call_id: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::User,
                parts: vec![MessagePart::ToolResult {
                    call_id: call_id.to_string(),
                    content: vec![querymt::chat::ToolResultPart::text("output")],
                    is_error: false,
                    tool_name: Some("shell".to_string()),
                    tool_arguments: Some("{}".to_string()),
                    compacted_at: None,
                }],
                created_at: 0,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            }
        }

        let messages = vec![
            // Old structured exchange: compacted away, including its calls.
            structured_assistant("a1", "s1", "call_old"),
            tool_result_user("r1", "s1", "call_old"),
            // Compaction pair.
            make_compaction_request_message("c1", "s1"),
            make_compaction_message("c2", "s1", "Summary"),
            // New structured exchange after compaction: kept intact.
            structured_assistant("a2", "s1", "call_new"),
            tool_result_user("r2", "s1", "call_new"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 4);
        assert_eq!(filtered[0].id, "c1");

        // The old canonical output is absent from effective replay.
        let serialized = serde_json::to_string(&filtered).unwrap();
        assert!(
            !serialized.contains("call_old"),
            "compacted structured exchange must not leak into effective history"
        );

        // Every kept tool result still has its matching call in a kept
        // assistant message: no dangling dependencies.
        let kept_call_ids: std::collections::HashSet<String> = filtered
            .iter()
            .flat_map(|m| m.function_calls())
            .map(|call| call.id)
            .collect();
        for msg in &filtered {
            for part in &msg.parts {
                if let MessagePart::ToolResult { call_id, .. } = part {
                    assert!(
                        kept_call_ids.contains(call_id.as_str()),
                        "tool result {call_id} has no matching call in effective history"
                    );
                }
            }
        }
    }

    #[test]
    fn test_filter_to_effective_history_only_returns_post_compaction() {
        // Simulate a second compaction scenario: old messages, compaction pair, new messages
        let messages = vec![
            MessageFixture::user_message("1", "s1", "Very old message"),
            MessageFixture::assistant_message("2", "s1", "Very old response"),
            MessageFixture::user_message("3", "s1", "Another old message"),
            MessageFixture::assistant_message("4", "s1", "Another old response"),
            MessageFixture::compaction_request_message("5", "s1"),
            MessageFixture::compaction_message("6", "s1", "Summary of old messages"),
            MessageFixture::user_message("7", "s1", "New message after compaction"),
            MessageFixture::assistant_message("8", "s1", "New response"),
            MessageFixture::user_message("9", "s1", "Latest message"),
        ];

        let filtered = filter_to_effective_history(messages);

        // Should contain compaction pair + everything after
        assert_eq!(filtered.len(), 5);
        assert_eq!(filtered[0].id, "5"); // CompactionRequest (user)
        assert_eq!(filtered[1].id, "6"); // Compaction (assistant)
        assert_eq!(filtered[2].id, "7");
        assert_eq!(filtered[3].id, "8");
        assert_eq!(filtered[4].id, "9");

        // Verify the pair starts with user→assistant
        assert_eq!(filtered[0].role, ChatRole::User);
        assert!(
            filtered[0]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::CompactionRequest { .. }))
        );
        assert_eq!(filtered[1].role, ChatRole::Assistant);
        assert!(
            filtered[1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::Compaction { .. }))
        );
    }

    #[test]
    fn test_merge_consecutive_tool_result_messages_combines_split_results() {
        // Simulates legacy DB layout: assistant with 3 tool_use, followed by
        // 3 separate User messages each with 1 tool_result.
        let messages = vec![
            MessageFixture::user_message("1", "s1", "do stuff"),
            make_assistant_tool_use("2", "s1"),
            make_user_tool_result("3", "s1", "call-a"),
            make_user_tool_result("4", "s1", "call-b"),
            make_user_tool_result("5", "s1", "call-c"),
            MessageFixture::assistant_message("6", "s1", "done"),
        ];

        let merged = merge_consecutive_tool_result_messages(messages);

        assert_eq!(merged.len(), 4, "3 tool_result msgs should merge into 1");
        // The merged message should keep the first message's id.
        assert_eq!(merged[2].id, "3");
        assert_eq!(merged[2].role, ChatRole::User);
        // All 3 tool_result parts should be in the merged message.
        let tr_count = merged[2]
            .parts
            .iter()
            .filter(|p| matches!(p, MessagePart::ToolResult { .. }))
            .count();
        assert_eq!(tr_count, 3, "merged message should have 3 ToolResult parts");
    }

    #[test]
    fn test_merge_consecutive_leaves_single_results_alone() {
        // Single tool_result User messages should not be affected.
        let messages = vec![
            MessageFixture::user_message("1", "s1", "hi"),
            make_assistant_tool_use("2", "s1"),
            make_user_tool_result("3", "s1", "call-a"),
            MessageFixture::assistant_message("4", "s1", "ok"),
        ];

        let merged = merge_consecutive_tool_result_messages(messages);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[2].parts.len(), 1);
    }

    #[test]
    fn test_merge_consecutive_does_not_merge_non_tool_result_user_messages() {
        // Two consecutive User messages where the second is plain text (not a
        // tool result) should not be merged.
        let messages = vec![
            make_assistant_tool_use("1", "s1"),
            make_user_tool_result("2", "s1", "call-a"),
            MessageFixture::user_message("3", "s1", "follow up"),
            MessageFixture::assistant_message("4", "s1", "ok"),
        ];

        let merged = merge_consecutive_tool_result_messages(messages);
        // No merging should happen — 2 and 3 stay separate.
        assert_eq!(merged.len(), 4);
    }

    #[test]
    fn test_filter_to_effective_history_merges_split_tool_results() {
        // End-to-end: filter_to_effective_history should also merge split
        // tool_result messages (safety net for existing DB data).
        let messages = vec![
            MessageFixture::user_message("1", "s1", "do stuff"),
            make_assistant_tool_use("2", "s1"),
            make_user_tool_result("3", "s1", "call-a"),
            make_user_tool_result("4", "s1", "call-b"),
            MessageFixture::assistant_message("5", "s1", "done"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(
            filtered.len(),
            4,
            "2 split tool_result msgs should be merged into 1"
        );
    }
}
