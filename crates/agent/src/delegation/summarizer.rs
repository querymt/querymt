//! Delegation summary generation
//!
//! This module provides functionality to generate an "Implementation Brief" from
//! a parent planning conversation before delegating to a coder agent. The brief
//! provides the coder with context about decisions made, files to modify, patterns
//! to follow, and implementation steps.

use crate::agent::utils::{render_prompt_for_display, render_prompt_for_llm};
use crate::config::DelegationSummaryConfig;
use crate::model::{AgentMessage, MessagePart};
use crate::session::error::{SessionError, SessionResult};
use crate::session::provider::build_provider_from_config;
use crate::session::pruning::{SimpleTokenEstimator, TokenEstimator};
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatRole};
use querymt::plugin::host::PluginRegistry;
use std::sync::Arc;
use std::time::Duration;

/// System prompt for the summarizer LLM
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

/// Summarizes a parent planning session for delegation handoff
pub struct DelegationSummarizer {
    provider: Arc<dyn LLMProvider>,
    timeout: Duration,
    min_history_tokens: usize,
    estimator: Arc<dyn TokenEstimator>,
}

impl DelegationSummarizer {
    /// Build a summarizer from configuration
    pub async fn from_config(
        config: &DelegationSummaryConfig,
        plugin_registry: Arc<PluginRegistry>,
    ) -> SessionResult<Self> {
        // Build params JSON including system prompt and max_tokens
        let mut params = serde_json::json!({
            "system": vec![SUMMARIZER_SYSTEM_PROMPT],
        });

        if let Some(max_tokens) = config.max_tokens {
            params["max_tokens"] = max_tokens.into();
        }

        let provider = build_provider_from_config(
            &plugin_registry,
            &config.provider,
            &config.model,
            Some(&params),
            config.api_key.as_deref(),
            #[cfg(feature = "remote")]
            None, // provider_node: summarizer always uses local provider
            #[cfg(feature = "remote")]
            None, // mesh_handle: not needed for summarizer
        )
        .await?;

        Ok(Self {
            provider,
            timeout: Duration::from_secs(config.timeout_secs),
            min_history_tokens: config.min_history_tokens,
            estimator: Arc::new(SimpleTokenEstimator),
        })
    }

    /// Generate a structured Implementation Brief from parent session history
    pub async fn summarize(
        &self,
        parent_history: &[AgentMessage],
        delegation_objective: &str,
    ) -> SessionResult<String> {
        // If the last message in history is a compaction (no messages after it),
        // its summary is already adequate — skip the LLM call.
        if let Some(summary) = Self::compaction_as_summary(parent_history) {
            log::info!("Using existing compaction summary for delegation (skipping LLM call)");
            return Ok(summary);
        }

        // Check token threshold — below threshold, inject raw formatted history
        let estimated_tokens = self.estimate_history_tokens(parent_history);
        if estimated_tokens < self.min_history_tokens {
            log::info!(
                "Parent history below summarization threshold ({} tokens < {}), injecting raw history",
                estimated_tokens,
                self.min_history_tokens
            );
            return Ok(self.prepare_input(parent_history, delegation_objective));
        }

        // 1. Prepare input from parent history
        let input = self.prepare_input(parent_history, delegation_objective);

        // 2. Call LLM with timeout
        let messages = vec![ChatMessage {
            role: ChatRole::User,
            message_type: querymt::chat::MessageType::Text,
            content: input,
            thinking: None,
            cache: None,
        }];

        let provider = self.provider.clone();
        let timeout = self.timeout;

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

        // 3. Extract text response
        let summary = response
            .text()
            .unwrap_or_else(|| "No summary generated".to_string());

        Ok(summary)
    }

    /// Estimate token count for a list of messages using the configured estimator
    fn estimate_history_tokens(&self, history: &[AgentMessage]) -> usize {
        history
            .iter()
            .map(|m| {
                m.parts
                    .iter()
                    .map(|p| match p {
                        MessagePart::Text { content } => self.estimator.estimate(content),
                        MessagePart::Prompt { blocks } => self
                            .estimator
                            .estimate(&render_prompt_for_llm(blocks, None)),
                        MessagePart::ToolResult { content, .. } => self.estimator.estimate(content),
                        MessagePart::Reasoning { content, .. } => self.estimator.estimate(content),
                        MessagePart::Compaction { summary, .. } => self.estimator.estimate(summary),
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum()
    }

    /// If the last message in history is a compaction (no messages after it),
    /// return its summary directly — it's already adequate context.
    fn compaction_as_summary(history: &[AgentMessage]) -> Option<String> {
        // Find the index of the last message that contains a Compaction part
        let last_compaction_idx = history.iter().rposition(|m| {
            m.parts
                .iter()
                .any(|p| matches!(p, MessagePart::Compaction { .. }))
        })?;

        // If there are messages after the compaction, we can't skip
        if last_compaction_idx < history.len() - 1 {
            return None;
        }

        // Extract the compaction summary text
        history[last_compaction_idx].parts.iter().find_map(|p| {
            if let MessagePart::Compaction { summary, .. } = p {
                Some(summary.clone())
            } else {
                None
            }
        })
    }

    /// Prepare the input for the summarizer LLM
    fn prepare_input(&self, history: &[AgentMessage], objective: &str) -> String {
        // Find the last compaction message — only process from there forward
        // This mirrors the pattern in SessionContext::history()
        let start_index = history
            .iter()
            .rposition(|m| {
                m.parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::Compaction { .. }))
            })
            .unwrap_or(0);

        let mut input = String::new();

        for msg in &history[start_index..] {
            match msg.role {
                ChatRole::User => {
                    // Include full user messages — they contain decisions and requirements
                    input.push_str(&format!("\n[User]: {}\n", Self::extract_text_content(msg)));
                }
                ChatRole::Assistant => {
                    for part in &msg.parts {
                        match part {
                            MessagePart::Text { content } => {
                                input.push_str(&format!("\n[Planner]: {}\n", content));
                            }
                            MessagePart::Prompt { blocks } => {
                                let display_content = render_prompt_for_display(blocks);
                                if !display_content.trim().is_empty() {
                                    input.push_str(&format!("\n[Planner]: {}\n", display_content));
                                }
                            }
                            MessagePart::ToolUse(tu) => {
                                // Just the tool name + key args, not full output
                                // Parse arguments JSON to extract meaningful info
                                let args_summary = if let Ok(args_value) =
                                    serde_json::from_str::<serde_json::Value>(
                                        &tu.function.arguments,
                                    ) {
                                    Self::summarize_tool_args(&args_value)
                                } else {
                                    tu.function.arguments.clone()
                                };
                                input.push_str(&format!(
                                    "\n[Tool Call]: {} ({})\n",
                                    tu.function.name, args_summary
                                ));
                            }
                            MessagePart::Compaction {
                                summary,
                                original_token_count: _,
                            } => {
                                // Include compaction summaries — they're already condensed
                                input.push_str(&format!(
                                    "\n[Previous Context Summary]: {}\n",
                                    summary
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        format!(
            r#"You are a technical brief writer. A planning agent had the following \
conversation while researching a task. The task will now be delegated \
to a coding agent for implementation.

Delegation objective: {objective}

Planning conversation:
{input}

Write a structured Implementation Brief for the coding agent. Include:
1. **Objective** — one clear sentence
2. **Key Decisions** — what was decided during planning
3. **Files to Modify** — specific file paths and what to change in each
4. **Patterns to Follow** — code patterns, conventions, or reference implementations found
5. **Constraints** — technical constraints, user preferences, things to avoid
6. **Implementation Steps** — ordered list of concrete steps

Be specific. Include file paths, function names, and code patterns \
the planner discovered. The coding agent has no access to this \
planning conversation."#
        )
    }

    /// Extract text content from all message parts
    fn extract_text_content(msg: &AgentMessage) -> String {
        let mut rendered_parts = Vec::new();
        for part in &msg.parts {
            match part {
                MessagePart::Text { content } => rendered_parts.push(content.clone()),
                MessagePart::Prompt { blocks } => {
                    rendered_parts.push(render_prompt_for_display(blocks));
                }
                _ => {}
            }
        }
        rendered_parts.join("\n")
    }

    /// Summarize tool arguments to just the key info
    fn summarize_tool_args(input: &serde_json::Value) -> String {
        // Extract common useful fields
        if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
            return path.to_string();
        }
        if let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) {
            return pattern.to_string();
        }
        if let Some(file_path) = input.get("filePath").and_then(|v| v.as_str()) {
            return file_path.to_string();
        }
        if let Some(command) = input.get("command").and_then(|v| v.as_str()) {
            return if command.len() > 100 {
                format!("{}...", &command[..100])
            } else {
                command.to_string()
            };
        }

        // Fallback: truncated JSON
        let s = serde_json::to_string(input).unwrap_or_default();
        if s.len() > 200 {
            format!("{}...", &s[..200])
        } else {
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── summarize_tool_args ────────────────────────────────────────────────

    #[test]
    fn summarize_tool_args_extracts_path() {
        let args = serde_json::json!({
            "path": "src/main.rs",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "src/main.rs");
    }

    #[test]
    fn summarize_tool_args_extracts_pattern() {
        let args = serde_json::json!({
            "pattern": "*.rs",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "*.rs");
    }

    #[test]
    fn summarize_tool_args_extracts_file_path() {
        let args = serde_json::json!({
            "filePath": "test.txt",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "test.txt");
    }

    #[test]
    fn summarize_tool_args_extracts_command() {
        let args = serde_json::json!({
            "command": "cargo build",
            "other": "ignored"
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary, "cargo build");
    }

    #[test]
    fn summarize_tool_args_truncates_long_command() {
        let long_cmd = "a".repeat(150);
        let args = serde_json::json!({
            "command": long_cmd
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary.len(), 103); // 100 chars + "..."
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn summarize_tool_args_fallback_to_json() {
        let args = serde_json::json!({
            "unknown_field": "value",
            "another": 123
        });
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        // Should be JSON representation
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
        let summary = DelegationSummarizer::summarize_tool_args(&args);
        assert_eq!(summary.len(), 203); // 200 chars + "..."
        assert!(summary.ends_with("..."));
    }

    // NOTE: Tests for compaction_as_summary, extract_text_content, and prepare_input
    // require proper AgentMessage construction with all required fields (id, session_id,
    // created_at, parent_message_id). These are better suited as integration tests
    // with proper test fixtures.
    //
    // The pure logic tests above (summarize_tool_args) cover the critical functionality
    // that doesn't require complex setup.
}
