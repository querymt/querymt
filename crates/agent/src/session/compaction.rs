//! AI-powered conversation compaction
//!
//! This module implements the AI compaction layer of the 3-layer compaction system,
//! which generates summaries when context threshold is reached.

use crate::agent::utils::render_prompt_for_llm;
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
        max_prompt_bytes: Option<usize>,
    ) -> Result<CompactionResult> {
        // Estimate original token count
        let original_token_count = self.estimate_messages_tokens(messages, max_prompt_bytes);

        // Build the compaction request
        let chat_messages = self.build_compaction_messages(messages, max_prompt_bytes);

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
        max_prompt_bytes: Option<usize>,
    ) -> Vec<querymt::chat::ChatMessage> {
        let mut chat_messages: Vec<querymt::chat::ChatMessage> = messages
            .iter()
            .map(|m| m.to_chat_message_with_max_prompt_bytes(max_prompt_bytes))
            .collect();

        // Add the compaction prompt as a user message
        chat_messages.push(querymt::chat::ChatMessage {
            role: ChatRole::User,
            message_type: querymt::chat::MessageType::Text,
            content: COMPACTION_PROMPT.to_string(),
            thinking: None,
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
    pub(crate) fn estimate_messages_tokens(
        &self,
        messages: &[AgentMessage],
        max_prompt_bytes: Option<usize>,
    ) -> usize {
        messages
            .iter()
            .map(|m| {
                m.parts
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { content } => self.estimator.estimate(content),
                        MessagePart::Prompt { blocks } => self
                            .estimator
                            .estimate(&render_prompt_for_llm(blocks, max_prompt_bytes)),
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
                    MessagePart::TurnSnapshotStart { .. } | MessagePart::TurnSnapshotPatch { .. }
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
    use crate::test_utils::mocks::MockCompactionProvider;
    use querymt::error::LLMError;

    // ========================================================================
    // Test Fixtures
    // ========================================================================

    /// Fixture for building test messages
    struct MessageFixture;

    impl MessageFixture {
        fn text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
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

        fn assistant_message(id: &str, session_id: &str, text: &str) -> AgentMessage {
            Self::text_message(id, session_id, ChatRole::Assistant, text)
        }

        fn user_message(id: &str, session_id: &str, text: &str) -> AgentMessage {
            Self::text_message(id, session_id, ChatRole::User, text)
        }

        fn tool_result_message(
            id: &str,
            session_id: &str,
            call_id: &str,
            content: &str,
        ) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::ToolResult {
                    call_id: call_id.to_string(),
                    content: content.to_string(),
                    is_error: false,
                    tool_name: Some("test_tool".to_string()),
                    tool_arguments: None,
                    compacted_at: None,
                }],
                created_at: 0,
                parent_message_id: None,
            }
        }

        fn compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
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

        fn reasoning_message(id: &str, session_id: &str, reasoning: &str) -> AgentMessage {
            AgentMessage {
                id: id.to_string(),
                session_id: session_id.to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Reasoning {
                    content: reasoning.to_string(),
                    time_ms: Some(100),
                }],
                created_at: 0,
                parent_message_id: None,
            }
        }

        /// Create a simple conversation with user/assistant exchanges
        fn simple_conversation(session_id: &str) -> Vec<AgentMessage> {
            vec![
                Self::user_message("1", session_id, "Hello"),
                Self::assistant_message("2", session_id, "Hi there"),
                Self::user_message("3", session_id, "How are you?"),
                Self::assistant_message("4", session_id, "I'm doing great!"),
            ]
        }

        /// Create a conversation with tool usage
        fn conversation_with_tools(session_id: &str) -> Vec<AgentMessage> {
            vec![
                Self::user_message("1", session_id, "List the files"),
                Self::tool_result_message("2", session_id, "call1", "file1.txt\nfile2.txt"),
                Self::user_message("3", session_id, "Read file1.txt"),
                Self::tool_result_message("4", session_id, "call2", "File contents here"),
            ]
        }

        /// Create a long conversation (for token counting tests)
        fn long_conversation(session_id: &str) -> Vec<AgentMessage> {
            let long_text = "a".repeat(1000);
            vec![
                Self::user_message("1", session_id, &long_text),
                Self::tool_result_message("2", session_id, "call1", &long_text),
                Self::user_message("3", session_id, &long_text),
                Self::tool_result_message("4", session_id, "call2", &long_text),
            ]
        }
    }

    /// Fixture for SessionCompaction service
    struct CompactionFixture {
        service: SessionCompaction,
    }

    impl CompactionFixture {
        fn new() -> Self {
            Self {
                service: SessionCompaction::new(),
            }
        }

        async fn process_with_summary(
            &self,
            messages: &[AgentMessage],
            summary_response: &str,
        ) -> Result<CompactionResult> {
            let mock = MockCompactionProvider::with_summary(summary_response);
            self.service
                .process(
                    messages,
                    Arc::new(mock),
                    "test-model",
                    &RetryConfig::default(),
                    None,
                )
                .await
        }
    }

    // ========================================================================
    // Helper Functions (backwards compatibility)
    // ========================================================================

    fn make_text_message(id: &str, session_id: &str, role: ChatRole, text: &str) -> AgentMessage {
        MessageFixture::text_message(id, session_id, role, text)
    }

    fn make_compaction_message(id: &str, session_id: &str, summary: &str) -> AgentMessage {
        MessageFixture::compaction_message(id, session_id, summary)
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

    // ========================================================================
    // New Unit Tests Using Fixtures
    // ========================================================================

    #[test]
    fn test_message_fixture_creates_text_messages() {
        let msg = MessageFixture::user_message("1", "sess1", "test");
        assert_eq!(msg.id, "1");
        assert_eq!(msg.session_id, "sess1");
        assert_eq!(msg.role, ChatRole::User);
        assert_eq!(msg.parts.len(), 1);
    }

    #[test]
    fn test_message_fixture_simple_conversation() {
        let conv = MessageFixture::simple_conversation("s1");
        assert_eq!(conv.len(), 4);
        assert_eq!(conv[0].role, ChatRole::User);
        assert_eq!(conv[1].role, ChatRole::Assistant);
    }

    #[test]
    fn test_estimate_messages_tokens_text_parts() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for text messages");
    }

    #[test]
    fn test_estimate_messages_tokens_tool_results() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::conversation_with_tools("s1");

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for tool results");
    }

    #[test]
    fn test_estimate_messages_tokens_mixed_parts() {
        let fixture = CompactionFixture::new();
        let messages = vec![
            MessageFixture::user_message("1", "s1", "User request"),
            MessageFixture::reasoning_message("2", "s1", "Let me think about this"),
            MessageFixture::tool_result_message("3", "s1", "c1", "Tool output"),
            MessageFixture::assistant_message("4", "s1", "Final response"),
        ];

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(tokens > 0, "Should count tokens for all part types");
    }

    #[test]
    fn test_estimate_messages_tokens_empty() {
        let fixture = CompactionFixture::new();
        let messages: Vec<AgentMessage> = vec![];

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert_eq!(tokens, 0, "Empty messages should have 0 tokens");
    }

    #[test]
    fn test_estimate_messages_tokens_long_conversation() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::long_conversation("s1");

        let tokens = fixture.service.estimate_messages_tokens(&messages, None);
        assert!(
            tokens > 500,
            "Long conversation should have significant token count"
        );
    }

    #[test]
    fn test_build_compaction_messages_appends_prompt() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture.service.build_compaction_messages(&messages, None);

        // Should have all original messages + compaction prompt
        assert_eq!(
            chat_messages.len(),
            messages.len() + 1,
            "Should append compaction prompt"
        );

        // Last message should be the compaction prompt
        assert_eq!(chat_messages.last().unwrap().role, ChatRole::User);
        assert!(
            chat_messages
                .last()
                .unwrap()
                .content
                .contains("conversation")
        );
    }

    #[test]
    fn test_build_compaction_messages_preserves_roles() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture.service.build_compaction_messages(&messages, None);

        // Check that roles are preserved (excluding the appended prompt)
        assert_eq!(chat_messages[0].role, ChatRole::User);
        assert_eq!(chat_messages[1].role, ChatRole::Assistant);
        assert_eq!(chat_messages[2].role, ChatRole::User);
        assert_eq!(chat_messages[3].role, ChatRole::Assistant);
    }

    #[tokio::test]
    async fn test_process_success_simple() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let result = fixture
            .process_with_summary(&messages, "This is a summary")
            .await
            .expect("process should succeed");

        assert_eq!(result.summary, "This is a summary");
        assert!(result.original_token_count > 0);
        assert!(result.summary_token_count > 0);
    }

    #[tokio::test]
    async fn test_process_success_with_tools() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::conversation_with_tools("s1");

        let result = fixture
            .process_with_summary(&messages, "Tool execution summary")
            .await
            .expect("process should succeed");

        assert!(!result.summary.is_empty());
        assert!(result.original_token_count > 0);
        assert!(result.summary_token_count > 0);
    }

    #[tokio::test]
    async fn test_process_returns_correct_token_counts() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::long_conversation("s1");

        let original_tokens = fixture.service.estimate_messages_tokens(&messages, None);
        let result = fixture
            .process_with_summary(&messages, "short")
            .await
            .expect("process should succeed");

        assert_eq!(result.original_token_count, original_tokens);
        // Summary "short" should have fewer tokens than original
        assert!(result.summary_token_count <= original_tokens);
    }

    #[tokio::test]
    async fn test_process_with_empty_messages() {
        let fixture = CompactionFixture::new();
        let messages: Vec<AgentMessage> = vec![];

        let result = fixture
            .process_with_summary(&messages, "empty summary")
            .await
            .expect("process should succeed with empty messages");

        assert_eq!(result.original_token_count, 0);
        assert!(!result.summary.is_empty());
    }

    #[tokio::test]
    async fn test_process_retries_on_llm_error() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        // Create a mock that fails twice then succeeds
        let mock = MockCompactionProvider::new(vec![
            Err(LLMError::GenericError("timeout".to_string())),
            Err(LLMError::GenericError("timeout".to_string())),
            Ok("recovered summary".to_string()),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                mock_arc.clone(),
                "model",
                &RetryConfig::default(),
                None,
            )
            .await
            .expect("should succeed after retries");

        assert_eq!(result.summary, "recovered summary");
        assert_eq!(mock_arc.call_count(), 3, "Should have retried twice");
    }

    #[tokio::test]
    async fn test_process_fails_after_max_retries() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        // Create a mock that always fails
        let mock = MockCompactionProvider::new(vec![
            Err(LLMError::GenericError("error1".to_string())),
            Err(LLMError::GenericError("error2".to_string())),
            Err(LLMError::GenericError("error3".to_string())),
            Err(LLMError::GenericError("error4".to_string())),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(
                &messages,
                mock_arc.clone(),
                "model",
                &RetryConfig::default(),
                None,
            )
            .await;

        assert!(result.is_err(), "should fail after max retries");
        // Default retry config has max_retries=3, so 4 calls (initial + 3 retries)
        assert_eq!(mock_arc.call_count(), 4);
    }

    #[tokio::test]
    async fn test_process_with_custom_retry_config() {
        let service = SessionCompaction::new();
        let messages = MessageFixture::simple_conversation("s1");

        let custom_config = RetryConfig {
            max_retries: 1,
            initial_backoff_ms: 10,
            backoff_multiplier: 1.0,
        };

        let mock = MockCompactionProvider::new(vec![
            Err(LLMError::GenericError("error1".to_string())),
            Err(LLMError::GenericError("error2".to_string())),
        ]);
        let mock_arc = Arc::new(mock);

        let result = service
            .process(&messages, mock_arc.clone(), "model", &custom_config, None)
            .await;

        assert!(result.is_err());
        // With max_retries=1, should have 2 calls (initial + 1 retry)
        assert_eq!(mock_arc.call_count(), 2);
    }

    #[test]
    fn test_create_compaction_message_structure() {
        let msg = SessionCompaction::create_compaction_message("session1", "Test summary", 5000);

        assert_eq!(msg.session_id, "session1");
        assert_eq!(msg.role, ChatRole::Assistant);
        assert_eq!(msg.parts.len(), 1);
        assert!(!msg.id.is_empty());
        assert!(msg.created_at > 0);
    }

    #[test]
    fn test_create_compaction_message_with_different_summaries() {
        let msg1 = SessionCompaction::create_compaction_message("s1", "Summary 1", 1000);
        let msg2 = SessionCompaction::create_compaction_message("s1", "Summary 2", 2000);

        assert_ne!(
            msg1.id, msg2.id,
            "Different messages should have different IDs"
        );

        if let MessagePart::Compaction {
            summary: s1,
            original_token_count: t1,
        } = &msg1.parts[0]
            && let MessagePart::Compaction {
                summary: s2,
                original_token_count: t2,
            } = &msg2.parts[0]
        {
            assert_ne!(s1, s2);
            assert_ne!(t1, t2);
        }
    }

    #[tokio::test]
    async fn test_process_message_order_preserved() {
        let fixture = CompactionFixture::new();
        let messages = MessageFixture::simple_conversation("s1");

        let chat_messages = fixture.service.build_compaction_messages(&messages, None);

        // Verify first message is the user message from conversation
        if let MessagePart::Text { content } = &messages[0].parts[0] {
            assert!(chat_messages[0].content.contains(content));
        }
    }
}
