//! Generalized brief generator for Work Packets.
//!
//! This module provides a `BriefGenerator` that can produce structured briefs
//! from session history in different modes: plan, handoff, implementation
//! brief, checkpoint, and delegation result.
//!
//! It generalizes the logic originally in `DelegationSummarizer`:
//! - Compaction-as-summary shortcut
//! - Token threshold for raw vs LLM summarization
//! - History formatting (conversation transcript)
//! - Tool argument summarization
//!
//! `DelegationSummarizer` delegates to this internally for backward
//! compatibility.

use crate::agent::utils::{render_prompt_for_display, render_prompt_for_llm};
use crate::model::{AgentMessage, MessagePart};
use crate::session::error::{SessionError, SessionResult};
use crate::session::provider::{ProviderRequest, SessionProvider};
use crate::session::pruning::{SimpleTokenEstimator, TokenEstimator};
use querymt::LLMProvider;
use querymt::chat::{ChatMessage, ChatRole};
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Brief mode
// ---------------------------------------------------------------------------

/// What kind of brief to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BriefMode {
    /// Structured implementation plan with phases, decisions, and open questions.
    Plan,
    /// Session handoff / continuation context for a future session.
    Handoff,
    /// Implementation brief for a coding agent (delegation input).
    ImplementationBrief,
    /// Progress checkpoint summarizing what changed since last checkpoint.
    Checkpoint,
    /// Result summary from a completed effort or delegation.
    DelegationResult,
}

impl BriefMode {
    /// System prompt for the summarizer LLM.
    fn system_prompt(&self) -> &'static str {
        match self {
            Self::Plan => PLAN_SYSTEM_PROMPT,
            Self::Handoff => HANDOFF_SYSTEM_PROMPT,
            Self::ImplementationBrief => BRIEF_SYSTEM_PROMPT,
            Self::Checkpoint => CHECKPOINT_SYSTEM_PROMPT,
            Self::DelegationResult => RESULT_SYSTEM_PROMPT,
        }
    }

    /// User-facing prompt template.
    ///
    /// `{conversation}` and `{objective}` are placeholders.
    fn user_prompt_template(&self) -> &'static str {
        match self {
            Self::Plan => PLAN_USER_TEMPLATE,
            Self::Handoff => HANDOFF_USER_TEMPLATE,
            Self::ImplementationBrief => BRIEF_USER_TEMPLATE,
            Self::Checkpoint => CHECKPOINT_USER_TEMPLATE,
            Self::DelegationResult => RESULT_USER_TEMPLATE,
        }
    }
}

// ---------------------------------------------------------------------------
// Mode-specific prompts
// ---------------------------------------------------------------------------

const BRIEF_SYSTEM_PROMPT: &str = r#"You are a technical brief writer for a software development team. Your job is to 
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

const PLAN_SYSTEM_PROMPT: &str = r#"You are a technical planning assistant. Your job is to read a conversation
between a user and an agent, then produce a structured Implementation Plan
that can be resumed later by another session.

Rules:
- Be specific: include file paths, function names, line numbers when available
- Be structured: organize into clear phases with dependencies
- Include decisions: capture WHY choices were made, not just WHAT
- Preserve uncertainty: include open questions and risks
- Include patterns: reference existing code that should be followed
- Skip meta-discussion: omit back-and-forth about the planning process itself"#;

const HANDOFF_SYSTEM_PROMPT: &str = r#"You are a session handoff writer. Your job is to read the current session
and produce a compact but complete continuation packet for a future session.

Rules:
- Be specific: include file paths, function names, commands used
- Be complete: a future session must be able to pick up where this left off
- Prioritize: current goal and next action come first
- Include context: capture decisions, constraints, and rationale
- Include blockers: note any unresolved issues or risks
- Skip meta-discussion: omit the planning process itself"#;

const CHECKPOINT_SYSTEM_PROMPT: &str = r#"You are a progress checkpoint writer. Your job is to summarize what has
changed in the current session since the last checkpoint or start of work.

Rules:
- Be specific: list exact files changed and what was done in each
- Be concise: focus on concrete changes, not discussion
- Include verification: note tests/builds run and their results
- Include blockers: note any current issues preventing progress
- Include next step: recommend the immediate next action"#;

const RESULT_SYSTEM_PROMPT: &str = r#"You are a result summarizer. Your job is to read a completed work session
and produce a concise summary of what was accomplished.

Rules:
- Be specific: list files modified, functions added/changed
- Be concise: focus on outcomes, not process
- Include verification: note whether tests/builds pass
- Include remaining: note any incomplete items or follow-up work needed"#;

// -- User prompt templates --

const BRIEF_USER_TEMPLATE: &str = r#"You are a technical brief writer. A planning agent had the following \
conversation while researching a task. The task will now be delegated \
to a coding agent for implementation.

{conversation}

Write a structured Implementation Brief for the coding agent. Include:
1. **Objective** — one clear sentence
2. **Key Decisions** — what was decided during planning
3. **Files to Modify** — specific file paths and what to change in each
4. **Patterns to Follow** — code patterns, conventions, or reference implementations found
5. **Constraints** — technical constraints, user preferences, things to avoid
6. **Implementation Steps** — ordered list of concrete steps

Be specific. Include file paths, function names, and code patterns \
the planner discovered. The coding agent has no access to this \
planning conversation."#;

const PLAN_USER_TEMPLATE: &str = r#"The user asked to create a plan. Here is the conversation so far:

{conversation}

Write a structured Implementation Plan. Include:
1. **Goal** — one clear sentence
2. **Implementation Phases** — ordered phases with dependencies
3. **Key Decisions** — what was decided and why
4. **Rejected Alternatives** — approaches considered but not chosen
5. **Relevant Files/Modules** — specific paths and what role they play
6. **Constraints** — technical constraints and things to avoid
7. **Open Questions** — unresolved issues that may affect the plan
8. **Verification Strategy** — how to validate each phase

Be specific. Include file paths, function names, and code patterns."#;

const HANDOFF_USER_TEMPLATE: &str = r#"The user wants a handoff packet for a future session. Here is the conversation:

{conversation}

Write a compact Handoff Packet. Include:
1. **Goal** — current objective
2. **Current State** — where things stand right now
3. **Completed Work** — what has been done
4. **Key Decisions** — decisions made and rationale
5. **Constraints** — technical constraints and things to avoid
6. **Relevant Files** — files that matter for continuing
7. **Known Risks** — potential issues
8. **Next Recommended Step** — the immediate next action

A future session should be able to pick up exactly where this left off."#;

const CHECKPOINT_USER_TEMPLATE: &str = r#"The user wants a progress checkpoint. Here is the conversation:

{conversation}

Write a structured Checkpoint. Include:
1. **What Was Completed** — concrete changes made
2. **Files Changed** — specific file paths and modifications
3. **Tests/Builds Run** — verification performed and results
4. **Current Status** — where things stand
5. **Blockers or Risks** — issues preventing progress
6. **Next Recommended Step** — the immediate next action"#;

const RESULT_USER_TEMPLATE: &str = r#"The user wants a result summary. Here is the completed work session:

{conversation}

Write a structured Result Summary. Include:
1. **Objective** — what was being done
2. **What Was Accomplished** — concrete outcomes
3. **Files Modified** — specific paths and changes
4. **Verification** — tests/builds run and results
5. **Remaining Work** — incomplete items or follow-up needed"#;

// ---------------------------------------------------------------------------
// BriefGenerator
// ---------------------------------------------------------------------------

/// Configuration for building a `BriefGenerator`.
#[derive(Debug, Clone)]
pub struct BriefGeneratorConfig {
    /// LLM provider name.
    pub provider: String,
    /// LLM model name.
    pub model: String,
    /// Optional API key override.
    pub api_key: Option<String>,
    /// Maximum tokens for the summary LLM call.
    pub max_tokens: Option<usize>,
    /// Timeout in seconds for the LLM call.
    pub timeout_secs: u64,
    /// Minimum estimated tokens before triggering LLM summarization.
    /// Below this threshold, raw formatted history is used directly.
    pub min_history_tokens: usize,
}

/// Generalized brief generator for Work Packets.
///
/// Supports multiple modes (plan, handoff, brief, checkpoint, result)
/// while sharing the same underlying history formatting, token estimation,
/// and LLM call infrastructure.
pub struct BriefGenerator {
    provider: Arc<dyn LLMProvider>,
    timeout: Duration,
    min_history_tokens: usize,
    estimator: Arc<dyn TokenEstimator>,
}

impl BriefGenerator {
    /// Build a brief generator from configuration.
    pub async fn from_config(
        config: &BriefGeneratorConfig,
        mode: BriefMode,
        session_provider: &SessionProvider,
    ) -> SessionResult<Self> {
        let mut params = serde_json::json!({
            "system": vec![mode.system_prompt()],
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

    /// Build a brief generator from raw components (for testing or advanced use).
    pub fn from_parts(
        provider: Arc<dyn LLMProvider>,
        timeout: Duration,
        min_history_tokens: usize,
    ) -> Self {
        Self {
            provider,
            timeout,
            min_history_tokens,
            estimator: Arc::new(SimpleTokenEstimator),
        }
    }

    /// Generate a structured brief from session history.
    #[tracing::instrument(
        name = "brief_generator.generate",
        skip(self, history),
        fields(
            mode = ?mode,
            objective = %objective,
            history_messages = history.len(),
            estimated_tokens = tracing::field::Empty,
            strategy = tracing::field::Empty,
            llm_duration_ms = tracing::field::Empty,
            output_bytes = tracing::field::Empty,
        )
    )]
    pub async fn generate(
        &self,
        history: &[AgentMessage],
        objective: &str,
        mode: BriefMode,
    ) -> SessionResult<String> {
        let span = tracing::Span::current();

        // Strategy 1: compaction shortcut
        if let Some(summary) = compaction_as_summary(history) {
            span.record("strategy", "compaction");
            span.record("output_bytes", summary.len() as u64);
            log::info!("Using existing compaction summary (skipping LLM call)");
            return Ok(summary);
        }

        // Estimate tokens
        let estimated_tokens = estimate_history_tokens(self.estimator.as_ref(), history);
        span.record("estimated_tokens", estimated_tokens as u64);

        // Strategy 2: raw injection below threshold
        if estimated_tokens < self.min_history_tokens {
            span.record("strategy", "raw");
            log::debug!(
                "History below summarization threshold ({} tokens < {}), injecting raw",
                estimated_tokens,
                self.min_history_tokens
            );
            let conversation = format_conversation(history, objective);
            span.record("output_bytes", conversation.len() as u64);
            return Ok(conversation);
        }

        // Strategy 3: LLM summarization
        span.record("strategy", "llm");

        let input = prepare_llm_input(history, objective, mode);
        let messages = vec![ChatMessage {
            role: ChatRole::User,
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
                    "Brief generation timed out after {} seconds",
                    timeout.as_secs()
                ))
            })?
            .map_err(|e| {
                SessionError::InvalidOperation(format!("Brief generation LLM call failed: {}", e))
            })?;
        span.record("llm_duration_ms", llm_start.elapsed().as_millis() as u64);

        let summary = response
            .text()
            .unwrap_or_else(|| "No summary generated".to_string());

        span.record("output_bytes", summary.len() as u64);
        Ok(summary)
    }
}

// ---------------------------------------------------------------------------
// Shared formatting utilities
// ---------------------------------------------------------------------------

/// If the last message in history is a compaction (no messages after it),
/// return its summary directly — it's already adequate context.
pub(crate) fn compaction_as_summary(history: &[AgentMessage]) -> Option<String> {
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

/// Estimate token count for a list of messages using the given estimator.
pub(crate) fn estimate_history_tokens(
    estimator: &dyn TokenEstimator,
    history: &[AgentMessage],
) -> usize {
    history
        .iter()
        .map(|m| {
            m.parts
                .iter()
                .map(|p| match p {
                    MessagePart::Text { content } => estimator.estimate(content),
                    MessagePart::Prompt { blocks } => {
                        estimator.estimate(&render_prompt_for_llm(blocks, None))
                    }
                    MessagePart::ToolResult { content, .. } => {
                        let text: String = content
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        estimator.estimate(&text)
                    }
                    MessagePart::Reasoning { content, .. } => estimator.estimate(content),
                    MessagePart::Compaction { summary, .. } => estimator.estimate(summary),
                    _ => 0,
                })
                .sum::<usize>()
        })
        .sum()
}

/// Format the conversation as a readable transcript.
///
/// This produces a clean context dump suitable for direct injection into
/// a delegate agent's context or for LLM summarization.
pub(crate) fn format_conversation(history: &[AgentMessage], objective: &str) -> String {
    let mut conversation = String::new();

    for msg in history {
        match msg.role {
            ChatRole::User => {
                // Include full user messages — they contain decisions and requirements
                conversation.push_str(&format!("\n[User]: {}\n", extract_text_content(msg)));
            }
            ChatRole::Assistant => {
                for part in &msg.parts {
                    match part {
                        MessagePart::Text { content } => {
                            conversation.push_str(&format!("\n[Agent]: {}\n", content));
                        }
                        MessagePart::Prompt { blocks } => {
                            let display_content = render_prompt_for_display(blocks);
                            if !display_content.trim().is_empty() {
                                conversation.push_str(&format!("\n[Agent]: {}\n", display_content));
                            }
                        }
                        MessagePart::ToolUse(tu) => {
                            // Just the tool name + key args, not full output
                            let args_summary = if let Ok(args_value) =
                                serde_json::from_str::<serde_json::Value>(&tu.function.arguments)
                            {
                                summarize_tool_args(&args_value)
                            } else {
                                tu.function.arguments.clone()
                            };
                            conversation.push_str(&format!(
                                "\n[Tool Call]: {} ({})\n",
                                tu.function.name, args_summary
                            ));
                        }
                        MessagePart::Compaction {
                            summary,
                            original_token_count: _,
                        } => {
                            // Include compaction summaries — they're already condensed
                            conversation
                                .push_str(&format!("\n[Previous Context Summary]: {}\n", summary));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    format!("Objective: {objective}\n\nConversation:\n{conversation}")
}

/// Prepare the full prompt for the summarizer LLM.
pub(crate) fn prepare_llm_input(
    history: &[AgentMessage],
    objective: &str,
    mode: BriefMode,
) -> String {
    let conversation = format_conversation(history, objective);
    mode.user_prompt_template()
        .replace("{conversation}", &conversation)
}

/// Extract text content from all message parts.
pub(crate) fn extract_text_content(msg: &AgentMessage) -> String {
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

/// Summarize tool arguments to just the key info.
pub(crate) fn summarize_tool_args(input: &serde_json::Value) -> String {
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
            let end = command.floor_char_boundary(100);
            format!("{}...", &command[..end])
        } else {
            command.to_string()
        };
    }

    // Fallback: truncated JSON
    let s = serde_json::to_string(input).unwrap_or_default();
    if s.len() > 200 {
        let end = s.floor_char_boundary(200);
        format!("{}...", &s[..end])
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    // ── format_conversation ────────────────────────────────────────────────

    fn make_user_msg(text: &str) -> AgentMessage {
        AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::User,
            parts: vec![MessagePart::Text {
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
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Text {
                content: text.to_string(),
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        }
    }

    #[test]
    fn format_conversation_basic() {
        let history = vec![
            make_user_msg("Add a login page"),
            make_assistant_msg("I'll create a login component in src/Login.tsx"),
        ];
        let output = format_conversation(&history, "Implement login page");

        assert!(output.contains("Objective: Implement login page"));
        assert!(output.contains("[User]: Add a login page"));
        assert!(output.contains("[Agent]: I'll create a login component"));
    }

    #[test]
    fn format_conversation_no_llm_instructions() {
        let history = vec![
            make_user_msg("Add a login page"),
            make_assistant_msg("I'll create a login component"),
        ];
        let output = format_conversation(&history, "Implement login page");

        // Must NOT contain any LLM instructions from templates
        assert!(
            !output.contains("Write a structured Implementation Brief"),
            "format_conversation should not contain LLM instructions"
        );
        assert!(
            !output.contains("You are a technical brief writer"),
            "format_conversation should not contain LLM role preamble"
        );
    }

    // ── prepare_llm_input ──────────────────────────────────────────────────

    #[test]
    fn prepare_llm_input_brief_mode_contains_instructions() {
        let history = vec![
            make_user_msg("Add a login page"),
            make_assistant_msg("I'll create a login component"),
        ];
        let output = prepare_llm_input(
            &history,
            "Implement login page",
            BriefMode::ImplementationBrief,
        );

        assert!(output.contains("[User]: Add a login page"));
        assert!(output.contains("Write a structured Implementation Brief"));
    }

    #[test]
    fn prepare_llm_input_plan_mode_contains_plan_instructions() {
        let history = vec![
            make_user_msg("Build auth system"),
            make_assistant_msg("Let me plan the approach"),
        ];
        let output = prepare_llm_input(&history, "Auth system", BriefMode::Plan);

        assert!(output.contains("[User]: Build auth system"));
        assert!(output.contains("Write a structured Implementation Plan"));
    }

    #[test]
    fn prepare_llm_input_handoff_mode_contains_handoff_instructions() {
        let history = vec![
            make_user_msg("Continue later"),
            make_assistant_msg("Saving state"),
        ];
        let output = prepare_llm_input(&history, "Session handoff", BriefMode::Handoff);

        assert!(output.contains("[User]: Continue later"));
        assert!(output.contains("Write a compact Handoff Packet"));
    }

    #[test]
    fn prepare_llm_input_checkpoint_mode_contains_checkpoint_instructions() {
        let history = vec![
            make_user_msg("Checkpoint progress"),
            make_assistant_msg("Halfway done"),
        ];
        let output = prepare_llm_input(&history, "Progress checkpoint", BriefMode::Checkpoint);

        assert!(output.contains("[User]: Checkpoint progress"));
        assert!(output.contains("Write a structured Checkpoint"));
    }

    #[test]
    fn prepare_llm_input_result_mode_contains_result_instructions() {
        let history = vec![
            make_user_msg("Summarize results"),
            make_assistant_msg("Done with changes"),
        ];
        let output = prepare_llm_input(&history, "Result summary", BriefMode::DelegationResult);

        assert!(output.contains("[User]: Summarize results"));
        assert!(output.contains("Write a structured Result Summary"));
    }

    // ── compaction_as_summary ──────────────────────────────────────────────

    #[test]
    fn compaction_as_summary_returns_none_when_no_compaction() {
        let history = vec![make_user_msg("Hello"), make_assistant_msg("Hi there")];
        assert!(compaction_as_summary(&history).is_none());
    }

    #[test]
    fn compaction_as_summary_returns_summary_when_last_is_compaction() {
        let compaction_msg = AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Previous work summary".to_string(),
                original_token_count: 1000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        let history = vec![make_user_msg("Hi"), compaction_msg];
        let result = compaction_as_summary(&history);
        assert_eq!(result.as_deref(), Some("Previous work summary"));
    }

    #[test]
    fn compaction_as_summary_returns_none_when_messages_after_compaction() {
        let compaction_msg = AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "s1".to_string(),
            role: ChatRole::Assistant,
            parts: vec![MessagePart::Compaction {
                summary: "Summary".to_string(),
                original_token_count: 1000,
            }],
            created_at: 0,
            parent_message_id: None,
            source_provider: None,
            source_model: None,
        };
        // Compaction is NOT the last message
        let history = vec![compaction_msg, make_user_msg("After compaction")];
        assert!(compaction_as_summary(&history).is_none());
    }

    // ── BriefMode prompt consistency ───────────────────────────────────────

    #[test]
    fn all_modes_have_templates() {
        for mode in [
            BriefMode::Plan,
            BriefMode::Handoff,
            BriefMode::ImplementationBrief,
            BriefMode::Checkpoint,
            BriefMode::DelegationResult,
        ] {
            assert!(
                !mode.system_prompt().is_empty(),
                "Mode {:?} should have a system prompt",
                mode
            );
            assert!(
                !mode.user_prompt_template().is_empty(),
                "Mode {:?} should have a user prompt template",
                mode
            );
            assert!(
                mode.user_prompt_template().contains("{conversation}"),
                "Mode {:?} template should contain {{conversation}} placeholder",
                mode
            );
        }
    }
}
