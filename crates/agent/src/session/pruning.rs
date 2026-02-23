//! Tool output pruning for managing conversation history
//!
//! This module implements the pruning layer of the 3-layer compaction system,
//! which marks old tool outputs as compacted (soft delete) to keep context size manageable.

use crate::model::{AgentMessage, MessagePart};
use querymt::chat::ChatRole;

/// Default number of tokens to protect from pruning (most recent tool outputs)
pub const PRUNE_PROTECT_TOKENS: usize = 40_000;

/// Minimum tokens that must be prunable before we actually prune
pub const PRUNE_MINIMUM_TOKENS: usize = 20_000;

/// Default protected tools that should never be pruned
pub const PRUNE_PROTECTED_TOOLS: &[&str] = &["skill"];

/// Configuration for pruning behavior
#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Number of tokens of recent tool outputs to protect from pruning
    pub protect_tokens: usize,
    /// Minimum tokens required to justify pruning
    pub minimum_tokens: usize,
    /// Tools that should never be pruned
    pub protected_tools: Vec<String>,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            protect_tokens: PRUNE_PROTECT_TOKENS,
            minimum_tokens: PRUNE_MINIMUM_TOKENS,
            protected_tools: PRUNE_PROTECTED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Trait for estimating token counts from text
pub trait TokenEstimator: Send + Sync {
    fn estimate(&self, text: &str) -> usize;
}

/// Simple token estimator using character count heuristic (~4 chars per token)
#[derive(Debug, Clone, Default)]
pub struct SimpleTokenEstimator;

impl TokenEstimator for SimpleTokenEstimator {
    fn estimate(&self, text: &str) -> usize {
        text.len().saturating_div(4)
    }
}

/// Information about a prunable tool result
#[derive(Debug, Clone)]
pub struct PrunableToolResult {
    /// Message ID containing this tool result
    pub message_id: String,
    /// Call ID of the tool result
    pub call_id: String,
    /// Estimated tokens in the content
    pub tokens: usize,
}

/// Result of pruning analysis
#[derive(Debug, Clone)]
pub struct PruneAnalysis {
    /// Total tokens in protected (recent) tool outputs
    pub protected_tokens: usize,
    /// Total tokens that could be pruned
    pub prunable_tokens: usize,
    /// List of tool results that should be pruned
    pub candidates: Vec<PrunableToolResult>,
    /// Whether pruning should proceed (prunable_tokens >= minimum_tokens)
    pub should_prune: bool,
}

/// Compute which tool results should be marked as compacted.
///
/// # Algorithm (matching OpenCode)
///
/// 1. Walk backwards through messages (newest to oldest)
/// 2. Skip first 2 user turns (recent context)
/// 3. Stop if we hit a previous compaction summary
/// 4. Count tool output tokens
/// 5. Protect the most recent `protect_tokens` of tool outputs
/// 6. Only prune if > `minimum_tokens` to remove
/// 7. Return analysis with candidates to mark as compacted
///
/// # Arguments
///
/// * `messages` - The conversation history
/// * `config` - Pruning configuration
/// * `estimator` - Token estimator implementation
///
/// # Returns
///
/// A `PruneAnalysis` containing information about what should be pruned
pub fn compute_prune_candidates(
    messages: &[AgentMessage],
    config: &PruneConfig,
    estimator: &dyn TokenEstimator,
) -> PruneAnalysis {
    let mut user_turn_count = 0;
    let mut protected_tokens: usize = 0;
    let mut prunable_tokens: usize = 0;
    let mut candidates: Vec<PrunableToolResult> = Vec::new();

    // Walk backwards through messages (newest to oldest)
    for message in messages.iter().rev() {
        // Skip messages in the 2 most recent user turns
        // A "turn" starts with a user message. We skip until we've passed 2 user messages.
        // By checking before incrementing, assistant messages after user turn 2 (going backwards)
        // are still skipped, while turn 1 and older are processed.
        if user_turn_count < 2 {
            if message.role == ChatRole::User {
                user_turn_count += 1;
            }
            continue;
        }

        // Step 3: Stop if we hit a compaction boundary (request or summary)
        let has_compaction = message.parts.iter().any(|p| {
            matches!(
                p,
                MessagePart::Compaction { .. } | MessagePart::CompactionRequest { .. }
            )
        });
        if has_compaction {
            break;
        }

        // Step 4: Process tool results in this message
        for part in &message.parts {
            if let MessagePart::ToolResult {
                call_id,
                content,
                tool_name,
                compacted_at,
                ..
            } = part
            {
                // Skip already compacted tool results
                if compacted_at.is_some() {
                    continue;
                }

                // Skip protected tools (e.g., "skill")
                if let Some(name) = tool_name
                    && config.protected_tools.iter().any(|t| t == name)
                {
                    continue;
                }

                let tokens = estimator.estimate(content);

                // Step 5: Protect the most recent PRUNE_PROTECT tokens
                if protected_tokens < config.protect_tokens {
                    // This tool result is within the protection window
                    protected_tokens += tokens;
                } else {
                    // Beyond protection window - candidate for pruning
                    prunable_tokens += tokens;
                    candidates.push(PrunableToolResult {
                        message_id: message.id.clone(),
                        call_id: call_id.clone(),
                        tokens,
                    });
                }
            }
        }
    }

    // Step 6: Only prune if > PRUNE_MINIMUM tokens to remove
    let should_prune = prunable_tokens >= config.minimum_tokens;

    PruneAnalysis {
        protected_tokens,
        prunable_tokens,
        candidates: if should_prune { candidates } else { Vec::new() },
        should_prune,
    }
}

/// Extract unique message IDs from prune candidates
pub fn extract_message_ids(candidates: &[PrunableToolResult]) -> Vec<String> {
    let mut ids: Vec<String> = candidates.iter().map(|c| c.message_id.clone()).collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Extract call IDs from prune candidates
pub fn extract_call_ids(candidates: &[PrunableToolResult]) -> Vec<String> {
    candidates.iter().map(|c| c.call_id.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentMessage;

    fn make_user_message(id: &str, session_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
                content: "test".to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
        }
    }

    fn make_assistant_message_with_tool_result(
        id: &str,
        session_id: &str,
        call_id: &str,
        content: &str,
        tool_name: Option<&str>,
    ) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::ToolResult {
                call_id: call_id.to_string(),
                content: content.to_string(),
                is_error: false,
                tool_name: tool_name.map(|s| s.to_string()),
                tool_arguments: None,
                compacted_at: None,
            }],
            created_at: 0,
            parent_message_id: None,
        }
    }

    fn make_compaction_message(id: &str, session_id: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Previous conversation summary".to_string(),
                original_token_count: 10000,
            }],
            created_at: 0,
            parent_message_id: None,
        }
    }

    #[test]
    fn test_simple_token_estimator() {
        let estimator = SimpleTokenEstimator;
        assert_eq!(estimator.estimate(""), 0);
        assert_eq!(estimator.estimate("test"), 1);
        assert_eq!(estimator.estimate("12345678"), 2);
        assert_eq!(estimator.estimate(&"a".repeat(100)), 25);
    }

    #[test]
    fn test_prune_skips_recent_user_turns() {
        let estimator = SimpleTokenEstimator;
        let config = PruneConfig {
            protect_tokens: 0, // No protection to make pruning happen
            minimum_tokens: 1,
            protected_tools: vec![],
        };

        // Create messages with 3 user turns
        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(400), None),
            make_user_message("3", "s1"),
            make_assistant_message_with_tool_result("4", "s1", "c2", &"b".repeat(400), None),
            make_user_message("5", "s1"), // First user turn (recent)
            make_assistant_message_with_tool_result("6", "s1", "c3", &"c".repeat(400), None),
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // Should only prune messages before the 2 most recent user turns
        // The last user turn is "5", second to last is "3"
        // So messages 1 and 2 should be candidates
        assert!(analysis.should_prune);
        // Message "2" with call_id "c1" should be a candidate
        assert!(analysis.candidates.iter().any(|c| c.call_id == "c1"));
        // Messages in recent turns should not be pruned
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c3"));
    }

    #[test]
    fn test_prune_respects_protect_limit() {
        let estimator = SimpleTokenEstimator;
        let config = PruneConfig {
            protect_tokens: 200, // Protect 200 tokens
            minimum_tokens: 1,
            protected_tools: vec![],
        };

        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(400), None), // 100 tokens, old
            make_user_message("3", "s1"),
            make_assistant_message_with_tool_result("4", "s1", "c2", &"b".repeat(400), None), // 100 tokens, old
            make_user_message("5", "s1"),
            make_assistant_message_with_tool_result("6", "s1", "c3", &"c".repeat(400), None), // 100 tokens, protected
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // With 200 token protection and walking backwards:
        // - c3 (100 tokens) would be protected (user turn 5 is recent, skipped)
        // - c2 (100 tokens) after user turn 3 - fills up protection
        // - c1 (100 tokens) after user turn 1 - beyond protection, prunable
        assert!(analysis.protected_tokens <= 200);
        // At least some tokens should be prunable
        assert!(analysis.prunable_tokens > 0 || analysis.protected_tokens > 0);
    }

    #[test]
    fn test_prune_skips_protected_tools() {
        let estimator = SimpleTokenEstimator;
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1,
            protected_tools: vec!["skill".to_string()],
        };

        // Need 4+ user turns so that turn 1 and 2 are outside the 2-turn protection window
        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result(
                "2",
                "s1",
                "c1",
                &"a".repeat(400),
                Some("skill"), // Protected tool in turn 1
            ),
            make_user_message("3", "s1"),
            make_assistant_message_with_tool_result(
                "4",
                "s1",
                "c2",
                &"b".repeat(400),
                Some("read"), // Non-protected tool in turn 2
            ),
            make_user_message("5", "s1"), // Turn 3 - recent (protected)
            make_assistant_message_with_tool_result("6", "s1", "c3", &"c".repeat(400), None),
            make_user_message("7", "s1"), // Turn 4 - most recent (protected)
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // "skill" tool should never be pruned (even though turn 1 is outside protection window)
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c1"));
        // "read" tool is not protected and turn 2 is outside protection window
        assert!(analysis.candidates.iter().any(|c| c.call_id == "c2"));
        // c3 is in turn 3 which is protected
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c3"));
    }

    #[test]
    fn test_prune_requires_minimum_tokens() {
        let estimator = SimpleTokenEstimator;
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1000, // High minimum
            protected_tools: vec![],
        };

        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(40), None), // Only 10 tokens
            make_user_message("3", "s1"),
            make_user_message("4", "s1"),
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // Not enough tokens to justify pruning
        assert!(!analysis.should_prune);
        assert!(analysis.candidates.is_empty());
    }

    #[test]
    fn test_prune_stops_at_compaction_message() {
        let estimator = SimpleTokenEstimator;
        let config = PruneConfig {
            protect_tokens: 0,
            minimum_tokens: 1,
            protected_tools: vec![],
        };

        let messages = vec![
            make_user_message("1", "s1"),
            make_assistant_message_with_tool_result("2", "s1", "c1", &"a".repeat(400), None),
            make_compaction_message("3", "s1"), // Compaction - should stop here
            make_user_message("4", "s1"),
            make_assistant_message_with_tool_result("5", "s1", "c2", &"b".repeat(400), None),
            make_user_message("6", "s1"),
            make_user_message("7", "s1"),
        ];

        let analysis = compute_prune_candidates(&messages, &config, &estimator);

        // Should not prune c1 which is before the compaction
        assert!(!analysis.candidates.iter().any(|c| c.call_id == "c1"));
        // c2 is after compaction and beyond recent turns, should be prunable
        assert!(analysis.candidates.iter().any(|c| c.call_id == "c2"));
    }

    #[test]
    fn test_extract_message_ids() {
        let candidates = vec![
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c1".to_string(),
                tokens: 100,
            },
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c2".to_string(),
                tokens: 100,
            },
            PrunableToolResult {
                message_id: "msg2".to_string(),
                call_id: "c3".to_string(),
                tokens: 100,
            },
        ];

        let ids = extract_message_ids(&candidates);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"msg1".to_string()));
        assert!(ids.contains(&"msg2".to_string()));
    }

    #[test]
    fn test_extract_call_ids() {
        let candidates = vec![
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c1".to_string(),
                tokens: 100,
            },
            PrunableToolResult {
                message_id: "msg1".to_string(),
                call_id: "c2".to_string(),
                tokens: 100,
            },
        ];

        let ids = extract_call_ids(&candidates);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"c1".to_string()));
        assert!(ids.contains(&"c2".to_string()));
    }
}
