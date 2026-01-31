//! AI-powered conversation compaction
//!
//! This module implements the AI compaction layer of the 3-layer compaction system,
//! which generates summaries when context threshold is reached.

use crate::model::{AgentMessage, MessagePart};
use crate::session::pruning::{SimpleTokenEstimator, TokenEstimator};
use anyhow::Result;
use querymt::chat::ChatRole;
use std::sync::Arc;
use std::time::Duration;

/// Default compaction prompt used to generate conversation summaries
pub const COMPACTION_PROMPT: &str = r#"Provide a detailed prompt for continuing our conversation above. Focus on:
- What was done
- What is currently being worked on
- Which files are being modified
- What needs to be done next
- Key user requests, constraints, or preferences that should persist

Be comprehensive but concise. This summary will replace the conversation history."#;

/// Configuration for retry behavior
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum retry attempts
    pub max_retries: usize,
    /// Initial backoff delay in milliseconds
    pub initial_backoff_ms: u64,
    /// Multiplier for exponential backoff
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff_ms: 1000,
            backoff_multiplier: 2.0,
        }
    }
}

/// Result of a compaction operation
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// The generated summary
    pub summary: String,
    /// Original token count before compaction
    pub original_token_count: usize,
    /// Estimated token count of the summary
    pub summary_token_count: usize,
}

/// Service for generating AI-powered conversation summaries
pub struct SessionCompaction {
    estimator: Arc<dyn TokenEstimator>,
}

impl Default for SessionCompaction {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionCompaction {
    /// Create a new SessionCompaction with default token estimator
    pub fn new() -> Self {
        Self {
            estimator: Arc::new(SimpleTokenEstimator),
        }
    }

    /// Create a new SessionCompaction with custom token estimator
    pub fn with_estimator(estimator: Arc<dyn TokenEstimator>) -> Self {
        Self { estimator }
    }

    /// Process messages and generate a compaction summary
    ///
    /// This method:
    /// 1. Builds a prompt with conversation history
    /// 2. Calls the LLM provider with retry logic
    /// 3. Returns the compaction result
    ///
    /// # Arguments
    ///
    /// * `messages` - The conversation history to summarize
    /// * `provider` - The LLM provider to use for summary generation
    /// * `retry_config` - Configuration for retry behavior
    pub async fn process(
        &self,
        messages: &[AgentMessage],
        provider: Arc<dyn querymt::chat::ChatProvider>,
        model: &str,
        retry_config: &RetryConfig,
    ) -> Result<CompactionResult> {
        // Estimate original token count
        let original_token_count = self.estimate_messages_tokens(messages);

        // Build the compaction request
        let chat_messages = self.build_compaction_messages(messages);

        // Call LLM with retry logic
        let summary = self
            .call_with_retry(&chat_messages, provider, model, retry_config)
            .await?;

        let summary_token_count = self.estimator.estimate(&summary);

        Ok(CompactionResult {
            summary,
            original_token_count,
            summary_token_count,
        })
    }

    /// Build chat messages for compaction request
    fn build_compaction_messages(
        &self,
        messages: &[AgentMessage],
    ) -> Vec<querymt::chat::ChatMessage> {
        let mut chat_messages: Vec<querymt::chat::ChatMessage> =
            messages.iter().map(|m| m.to_chat_message()).collect();

        // Add the compaction prompt as a user message
        chat_messages.push(querymt::chat::ChatMessage {
            role: ChatRole::User,
            message_type: querymt::chat::MessageType::Text,
            content: COMPACTION_PROMPT.to_string(),
            cache: None,
        });

        chat_messages
    }

    /// Call LLM with exponential backoff retry
    async fn call_with_retry(
        &self,
        messages: &[querymt::chat::ChatMessage],
        provider: Arc<dyn querymt::chat::ChatProvider>,
        _model: &str,
        retry_config: &RetryConfig,
    ) -> Result<String> {
        let mut last_error = None;
        let mut backoff_ms = retry_config.initial_backoff_ms;

        for attempt in 0..=retry_config.max_retries {
            if attempt > 0 {
                log::debug!(
                    "Compaction retry attempt {} after {}ms",
                    attempt,
                    backoff_ms
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms as f64 * retry_config.backoff_multiplier) as u64;
            }

            // Note: The model is configured in the provider, we just call chat
            match provider.chat(messages).await {
                Ok(response) => {
                    return Ok(response.text().unwrap_or_default());
                }
                Err(e) => {
                    log::warn!(
                        "Compaction LLM call failed (attempt {}): {}",
                        attempt + 1,
                        e
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(anyhow::anyhow!(
            "Compaction failed after {} retries: {:?}",
            retry_config.max_retries,
            last_error
        ))
    }

    /// Estimate token count for a list of messages
    fn estimate_messages_tokens(&self, messages: &[AgentMessage]) -> usize {
        messages
            .iter()
            .map(|m| {
                m.parts
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { content } => self.estimator.estimate(content),
                        MessagePart::ToolResult { content, .. } => self.estimator.estimate(content),
                        MessagePart::Reasoning { content, .. } => self.estimator.estimate(content),
                        MessagePart::Compaction { summary, .. } => self.estimator.estimate(summary),
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    /// Create a compaction message to be stored in history
    pub fn create_compaction_message(
        session_id: &str,
        summary: &str,
        original_token_count: usize,
    ) -> AgentMessage {
        AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: summary.to_string(),
                original_token_count,
            }],
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        }
    }
}

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
    // Find the index of the last compaction message
    let last_compaction_idx = messages.iter().rposition(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Compaction { .. }))
    });

    let filtered: Vec<AgentMessage> = match last_compaction_idx {
        Some(idx) => messages.into_iter().skip(idx).collect(),
        None => messages, // No compaction found, return all messages
    };

    // Filter out messages that only contain snapshot metadata parts
    // These are for undo/redo tracking and should not be sent to the LLM
    // Keeping them creates empty messages that break tool_use -> tool_result sequencing
    filtered
        .into_iter()
        .filter(|m| {
            m.parts.iter().any(|p| {
                !matches!(
                    p,
                    MessagePart::StepSnapshotStart { .. } | MessagePart::StepSnapshotPatch { .. }
                )
            })
        })
        .collect()
}

/// Check if messages contain a compaction summary
pub fn has_compaction(messages: &[AgentMessage]) -> bool {
    messages.iter().any(|m| {
        m.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Compaction { .. }))
    })
}

/// Get the most recent compaction summary if it exists
pub fn get_last_compaction(messages: &[AgentMessage]) -> Option<&MessagePart> {
    for message in messages.iter().rev() {
        for part in &message.parts {
            if matches!(part, MessagePart::Compaction { .. }) {
                return Some(part);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role,
            parts: vec![MessagePart::Text {
                content: text.to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
        }
    }

    fn make_compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: summary.to_string(),
                original_token_count: 1000,
            }],
            created_at: 0,
            parent_message_id: None,
        }
    }

    #[test]
    fn test_filter_returns_all_if_no_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Hello"),
            make_text_message("2", "s1", ChatRole::Assistant, "Hi there"),
            make_text_message("3", "s1", ChatRole::User, "How are you?"),
        ];

        let filtered = filter_to_effective_history(messages.clone());
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].id, "1");
    }

    #[test]
    fn test_filter_returns_after_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Old message"),
            make_text_message("2", "s1", ChatRole::Assistant, "Old response"),
            make_compaction_message("3", "s1", "Summary of previous conversation"),
            make_text_message("4", "s1", ChatRole::User, "New message"),
            make_text_message("5", "s1", ChatRole::Assistant, "New response"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].id, "3"); // Compaction message
        assert_eq!(filtered[1].id, "4");
        assert_eq!(filtered[2].id, "5");
    }

    #[test]
    fn test_filter_uses_last_compaction() {
        let messages = vec![
            make_text_message("1", "s1", ChatRole::User, "Very old"),
            make_compaction_message("2", "s1", "First summary"),
            make_text_message("3", "s1", ChatRole::User, "Medium old"),
            make_compaction_message("4", "s1", "Second summary"),
            make_text_message("5", "s1", ChatRole::User, "Recent"),
        ];

        let filtered = filter_to_effective_history(messages);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].id, "4"); // Second compaction
        assert_eq!(filtered[1].id, "5");
    }

    #[test]
    fn test_has_compaction() {
        let messages_with = vec![
            make_text_message("1", "s1", ChatRole::User, "Hello"),
            make_compaction_message("2", "s1", "Summary"),
        ];

        let messages_without = vec![
            make_text_message("1", "s1", ChatRole::User, "Hello"),
            make_text_message("2", "s1", ChatRole::Assistant, "Hi"),
        ];

        assert!(has_compaction(&messages_with));
        assert!(!has_compaction(&messages_without));
    }

    #[test]
    fn test_get_last_compaction() {
        let messages = vec![
            make_compaction_message("1", "s1", "First summary"),
            make_text_message("2", "s1", ChatRole::User, "Hello"),
            make_compaction_message("3", "s1", "Second summary"),
        ];

        let last = get_last_compaction(&messages);
        assert!(last.is_some());
        if let Some(MessagePart::Compaction { summary, .. }) = last {
            assert_eq!(summary, "Second summary");
        } else {
            panic!("Expected Compaction part");
        }
    }

    #[test]
    fn test_create_compaction_message() {
        let msg = SessionCompaction::create_compaction_message("session1", "Test summary", 5000);

        assert_eq!(msg.session_id, "session1");
        assert_eq!(msg.role, ChatRole::Assistant);
        assert_eq!(msg.parts.len(), 1);

        if let MessagePart::Compaction {
            summary,
            original_token_count,
        } = &msg.parts[0]
        {
            assert_eq!(summary, "Test summary");
            assert_eq!(*original_token_count, 5000);
        } else {
            panic!("Expected Compaction part");
        }
    }

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_backoff_ms, 1000);
        assert!((config.backoff_multiplier - 2.0).abs() < f64::EPSILON);
    }
}
