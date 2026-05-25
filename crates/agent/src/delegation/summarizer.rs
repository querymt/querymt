//! Delegation summary generation
//!
//! This module provides functionality to generate an "Implementation Brief" from
//! a parent planning conversation before delegating to a coder agent. The brief
//! provides the coder with context about decisions made, files to modify, patterns
//! to follow, and implementation steps.
//!
//! The heavy lifting (history formatting, token estimation, compaction detection,
//! LLM call logic) is delegated to the shared `BriefGenerator` infrastructure in
//! `crate::work_packet::brief_generator`. This module keeps the `DelegationSummarizer`
//! public API intact for backward compatibility while sharing the underlying logic.

use crate::config::DelegationSummaryConfig;
use crate::model::AgentMessage;
use crate::session::error::{SessionError, SessionResult};
use crate::session::provider::{ProviderRequest, SessionProvider};
use crate::session::pruning::SimpleTokenEstimator;
use crate::work_packet::brief_generator::BriefMode;
use querymt::LLMProvider;
use std::sync::Arc;
use std::time::Duration;
use tracing::instrument;

/// System prompt for the summarizer LLM.
///
/// Kept as a constant for backward compatibility with tests that assert on it.
const SUMMARIZER_SYSTEM_PROMPT: &str = r#"You are a technical brief writer for a software development team. Your job is to 
read a planning conversation between a user and a planning agent, then produce a 
concise, structured Implementation Brief for a coding agent who will do the actual 
implementation.

Rules:
- Be specific: include file paths, function names, line numbers when available
- Be concise: the coding agent has limited context window
- Prioritize: what matters most for implementation comes first
- Include decisions: capture WHY choices were made, not just WHAT
- Include patterns: reference existing code the coder should follow
- Skip meta-discussion: omit back-and-forth about planning process itself"#;

/// Summarizes a parent planning session for delegation handoff.
///
/// Internally delegates to the shared `BriefGenerator` infrastructure, using
/// `BriefMode::ImplementationBrief` mode. This keeps the public API stable
/// while sharing formatting, token estimation, and LLM call logic.
pub struct DelegationSummarizer {
    provider: Arc<dyn LLMProvider>,
    timeout: Duration,
    min_history_tokens: usize,
    estimator: Arc<dyn crate::session::pruning::TokenEstimator>,
}

impl DelegationSummarizer {
    /// Build a summarizer from configuration
    pub async fn from_config(
        config: &DelegationSummaryConfig,
        session_provider: &SessionProvider,
    ) -> SessionResult<Self> {
        // Build params JSON including system prompt and max_tokens
        let mut params = serde_json::json!({
            "system": vec![SUMMARIZER_SYSTEM_PROMPT],
        });

        if let Some(max_tokens) = config.max_tokens {
            params["max_tokens"] = max_tokens.into();
        }

        let provider = session_provider
            .build_provider(
                ProviderRequest::new(&config.provider, &config.model)
                    .with_params(Some(&params))
                    .with_api_key_override(config.api_key.as_deref()),
            )
            .await?;

        Ok(Self {
            provider,
            timeout: Duration::from_secs(config.timeout_secs),
            min_history_tokens: config.min_history_tokens,
            estimator: Arc::new(SimpleTokenEstimator),
        })
    }

    /// Generate a structured Implementation Brief from parent session history.
    ///
    /// Uses the shared brief generation logic with `BriefMode::ImplementationBrief`.
    #[instrument(
        name = "delegation.summarizer.summarize",
        skip(self, parent_history),
        fields(
            objective = %delegation_objective,
            history_messages = parent_history.len(),
            estimated_tokens = tracing::field::Empty,
            strategy = tracing::field::Empty,
            llm_duration_ms = tracing::field::Empty,
            output_bytes = tracing::field::Empty,
        )
    )]
    pub async fn summarize(
        &self,
        parent_history: &[AgentMessage],
        delegation_objective: &str,
    ) -> SessionResult<String> {
        let span = tracing::Span::current();

        use crate::work_packet::brief_generator::{compaction_as_summary, estimate_history_tokens};

        // If the last message in history is a compaction (no messages after it),
        // its summary is already adequate — skip the LLM call.
        if let Some(summary) = compaction_as_summary(parent_history) {
            span.record("strategy", "compaction");
            span.record("output_bytes", summary.len() as u64);
            log::info!("Using existing compaction summary for delegation (skipping LLM call)");
            return Ok(summary);
        }

        // Check token threshold — below threshold, inject raw formatted history
        // directly into the delegate context (no LLM summarization needed)
        let estimated_tokens = estimate_history_tokens(&*self.estimator, parent_history);
        span.record("estimated_tokens", estimated_tokens as u64);
        if estimated_tokens < self.min_history_tokens {
            span.record("strategy", "raw");
            log::debug!(
                "Parent history below summarization threshold ({} tokens < {}), injecting raw history",
                estimated_tokens,
                self.min_history_tokens
            );
            // Use "Delegation objective" prefix for backward compatibility
            let result = Self::format_conversation_delegated(parent_history, delegation_objective);
            span.record("output_bytes", result.len() as u64);
            return Ok(result);
        }

        span.record("strategy", "llm");

        // 1. Prepare LLM prompt from parent history
        let input = Self::prepare_llm_input_delegated(parent_history, delegation_objective);

        // 2. Call LLM with timeout
        let messages = vec![querymt::chat::ChatMessage {
            role: querymt::chat::ChatRole::User,
            content: vec![querymt::chat::Content::text(input)],
            cache: None,
        }];

        let provider = self.provider.clone();
        let timeout = self.timeout;

        let llm_start = std::time::Instant::now();
        let response = tokio::time::timeout(timeout, async move { provider.chat(&messages).await })
            .await
            .map_err(|_| {
                SessionError::InvalidOperation(format!(
                    "Delegation summary generation timed out after {} seconds",
                    timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                SessionError::InvalidOperation(format!("Delegation summary LLM call failed: {}", e))
            })?;
        span.record("llm_duration_ms", llm_start.elapsed().as_millis() as u64);

        // 3. Extract text response
        let summary = response
            .text()
            .unwrap_or_else(|| "No summary generated".to_string());

        span.record("output_bytes", summary.len() as u64);
        Ok(summary)
    }

    /// Format the conversation using the shared utility with "Delegation objective" prefix
    /// for backward compatibility with existing tests.
    fn format_conversation_delegated(history: &[AgentMessage], objective: &str) -> String {
        use crate::work_packet::brief_generator::format_conversation;
        let result = format_conversation(history, objective);
        // The shared utility uses "Objective:" prefix; delegation uses
        // "Delegation objective:" for backward compatibility.
        result.replace("Objective: ", "Delegation objective: ")
    }

    /// Prepare LLM input using the shared utility with "Delegation objective" prefix
    /// for backward compatibility.
    fn prepare_llm_input_delegated(history: &[AgentMessage], objective: &str) -> String {
        use crate::work_packet::brief_generator::prepare_llm_input;
        let result = prepare_llm_input(history, objective, BriefMode::ImplementationBrief);
        // The shared utility uses "Objective:" prefix; delegation uses
        // "Delegation objective:" for backward compatibility.
        result.replace("Objective: ", "Delegation objective: ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_packet::brief_generator::summarize_tool_args;

    // ── summarize_tool_args (re-exported from brief_generator) ────────────

    #[test]
    fn summarize_tool_args_extracts_path() {
        let args = serde_json::json!({
            "path": "src/main.rs",
            "other": "ignored"
        });
        let summary = summarize_tool_args(&args);
        assert_eq!(summary, "src/main.rs");
    }

    #[test]
    fn summarize_tool_args_extracts_pattern() {
        let args = serde_json::json!({
            "pattern": "*.rs",
            "other": "ignored"
        });
        let summary = summarize_tool_args(&args);
        assert_eq!(summary, "*.rs");
    }

    #[test]
    fn summarize_tool_args_extracts_file_path() {
        let args = serde_json::json!({
            "filePath": "test.txt",
            "other": "ignored"
        });
        let summary = summarize_tool_args(&args);
        assert_eq!(summary, "test.txt");
    }

    #[test]
    fn summarize_tool_args_extracts_command() {
        let args = serde_json::json!({
            "command": "cargo build",
            "other": "ignored"
        });
        let summary = summarize_tool_args(&args);
        assert_eq!(summary, "cargo build");
    }

    #[test]
    fn summarize_tool_args_truncates_long_command() {
        let long_cmd = "a".repeat(150);
        let args = serde_json::json!({
            "command": long_cmd
        });
        let summary = summarize_tool_args(&args);
        assert!(summary.len() <= 103);
        assert!(summary.ends_with("..."));
        assert!(std::str::from_utf8(summary.trim_end_matches("...").as_bytes()).is_ok());
    }

    #[test]
    fn summarize_tool_args_truncates_multibyte_json() {
        let filler = "x".repeat(195);
        let args = serde_json::json!({
            "todos": [{"id": "1", "content": format!("{}—suffix", filler)}]
        });
        let summary = summarize_tool_args(&args);
        assert!(summary.ends_with("..."));
        assert!(summary.len() <= 203);
        assert!(std::str::from_utf8(summary.as_bytes()).is_ok());
    }

    #[test]
    fn summarize_tool_args_fallback_to_json() {
        let args = serde_json::json!({
            "unknown_field": "value",
            "another": 123
        });
        let summary = summarize_tool_args(&args);
        assert!(summary.contains("unknown_field"));
        assert!(summary.contains("value"));
    }

    #[test]
    fn summarize_tool_args_truncates_long_json() {
        let mut obj = serde_json::Map::new();
        for i in 0..50 {
            obj.insert(format!("field_{}", i), serde_json::json!("long_value"));
        }
        let args = serde_json::Value::Object(obj);
        let summary = summarize_tool_args(&args);
        assert!(summary.len() <= 203);
        assert!(summary.ends_with("..."));
        assert!(std::str::from_utf8(summary.trim_end_matches("...").as_bytes()).is_ok());
    }

    // ── format_conversation / prepare_llm_input ──────────────────────────────

    fn make_user_msg(text: &str) -> AgentMessage {
        AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: querymt::chat::ChatRole::User,
            parts: vec![crate::model::MessagePart::Text {
                content: text.to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    fn make_assistant_msg(text: &str) -> AgentMessage {
        AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: querymt::chat::ChatRole::Assistant,
            parts: vec![crate::model::MessagePart::Text {
                content: text.to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    #[test]
    fn format_conversation_does_not_contain_llm_instructions() {
        let history = vec![
            make_user_msg("Add a login page"),
            make_assistant_msg("I'll create a login component in src/Login.tsx"),
        ];
        let output =
            DelegationSummarizer::format_conversation_delegated(&history, "Implement login page");

        assert!(output.contains("Delegation objective: Implement login page"));
        assert!(output.contains("[User]: Add a login page"));
        assert!(output.contains("[Agent]: I'll create a login component"));
        // Must NOT contain summarizer LLM instructions
        assert!(
            !output.contains("Write a structured Implementation Brief"),
            "format_conversation should not contain LLM instructions"
        );
        assert!(
            !output.contains("You are a technical brief writer"),
            "format_conversation should not contain LLM role preamble"
        );
    }

    #[test]
    fn prepare_llm_input_contains_instructions() {
        let history = vec![
            make_user_msg("Add a login page"),
            make_assistant_msg("I'll create a login component"),
        ];
        let output =
            DelegationSummarizer::prepare_llm_input_delegated(&history, "Implement login page");

        // Should contain the conversation content
        assert!(output.contains("[User]: Add a login page"));
        // Should contain LLM instructions
        assert!(output.contains("Write a structured Implementation Brief"));
        assert!(output.contains("You are a technical brief writer"));
    }
}
