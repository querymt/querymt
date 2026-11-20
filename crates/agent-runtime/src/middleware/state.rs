use querymt::chat::{ChatMessage, ChatOutput, ChatRole, FinishReason, GenerationOptions};
use std::sync::Arc;

use querymt_agent_core::events::StopType;
use querymt_agent_core::model::{MessagePart, NoticeSource};

/// Statistics about agent execution
#[derive(Debug, Clone, Default)]
pub struct AgentStats {
    pub steps: usize,
    pub turns: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub context_tokens: usize,
    /// Known recorded spend of this cycle in USD: the sum of settled attempt
    /// valuations (see [`AgentStats::apply_recorded_costs`]), never a
    /// revaluation of the token counters.
    pub total_cost_usd: f64,
    /// Input cost in USD
    pub input_cost_usd: f64,
    /// Output cost in USD
    pub output_cost_usd: f64,
    /// Reasoning/thinking output tokens
    pub reasoning_tokens: u64,
    /// Cache read tokens (for providers that support prompt caching)
    pub cache_read_tokens: u64,
    /// Cache write tokens (for providers that support prompt caching)
    pub cache_write_tokens: u64,
    /// Cache read cost in USD
    pub cache_read_cost_usd: f64,
    /// Cache write cost in USD
    pub cache_write_cost_usd: f64,
    /// Known budget-view spend of this cycle (manual-rate valuation where
    /// snapshotted, otherwise the reported API valuation), including cache
    /// and auxiliary attempts. Read by price-limit middleware.
    pub budget_cost_usd: f64,
}

impl AgentStats {
    /// Present this cycle's recorded charges.
    ///
    /// The fields are a read-only view of settled accounting components, not
    /// a valuation: nothing here prices token counters, so a model switch or
    /// catalog refresh cannot change an earlier charge. Unknown components
    /// contribute nothing; completeness lives in [`ScopeTotals`].
    pub fn apply_recorded_costs(
        &mut self,
        totals: &querymt_agent_services::accounting::ScopeTotals,
    ) {
        let c = &totals.components;
        self.total_cost_usd = totals.known_subtotal_usd;
        self.budget_cost_usd = totals.budget_known_subtotal_usd;
        self.input_cost_usd = c.input_usd;
        self.output_cost_usd = c.output_usd;
        self.cache_read_cost_usd = c.cache_read_usd;
        self.cache_write_cost_usd = c.cache_write_usd + c.cache_write_1h_usd;
    }
}

/// Fold one request's token usage into `stats`.
///
/// Token counters only: monetary fields are a view of recorded accounting
/// components (see [`AgentStats::apply_recorded_costs`]), never a revaluation.
///
/// Shared by the normal reply path and by discarded attempts (for example an
/// escalated retry), so every request that was billed is counted exactly once.
/// Does not touch `steps`; callers decide whether the attempt counts as a step.
pub fn apply_usage_to_stats(stats: &mut AgentStats, usage: Option<&querymt::Usage>) {
    let Some(usage) = usage else {
        return;
    };
    stats.total_input_tokens += u64::from(usage.input_tokens);
    stats.total_output_tokens += u64::from(usage.output_tokens);
    stats.reasoning_tokens += u64::from(usage.reasoning_tokens);
    stats.cache_read_tokens += u64::from(usage.cache_read);
    stats.cache_write_tokens += u64::from(usage.cache_write);
    stats.context_tokens = calculate_context_tokens(Some(usage)) as usize;
}

/// Calculate current context size from usage.
/// Single source of truth for context token calculation.
/// Returns the sum of input, output, reasoning, cache_read and cache_write tokens, or 0 if no usage provided.
pub fn calculate_context_tokens(usage: Option<&querymt::Usage>) -> u64 {
    usage
        .map(|u| {
            (u.input_tokens + u.output_tokens + u.reasoning_tokens + u.cache_read + u.cache_write)
                as u64
        })
        .unwrap_or(0)
}

/// Represents a tool call
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
    /// The provider marked this call's input invalid (for example a block cut
    /// off by a token limit), even if `function.arguments` parses. Read from the
    /// canonical function-call item before the lossy projection to
    /// `querymt::ToolCall`. Such a call is rejected before hooks or invocation.
    pub invalid_input: bool,
}

#[derive(Debug, Clone)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

/// Harness text waiting to be stored as a notice before the next model request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotice {
    pub source: NoticeSource,
    pub content: String,
}

/// Context passed to middleware during state transitions
#[derive(Debug, Clone)]
pub struct PreparedModelRequest {
    pub messages: Arc<[ChatMessage]>,
    pub tools: Arc<[querymt::chat::Tool]>,
    pub estimated_tokens: usize,
    /// Per-request generation overrides copied from
    /// [`ConversationContext::request_overrides`].
    pub options: GenerationOptions,
}

/// Per-session facts about the model a session talks to.
///
/// Resolved together with the provider, replaced when the model is switched and
/// carried on [`ConversationContext`], so middleware never has to reconstruct
/// them from "the last request".
#[derive(Debug, Clone, Default)]
pub struct ModelProfile {
    /// Initial output cap and escalation ceiling; `None` when unknown.
    pub caps: Option<querymt_agent_services::model_heuristics::OutputCaps>,
    /// What the session's provider supports per request.
    pub capabilities: querymt::chat::ChatCapabilities,
    /// Reasoning effort the provider is configured with, when known.
    pub reasoning_effort: Option<querymt::chat::ReasoningEffort>,
}

/// Context passed to middleware during state transitions
#[derive(Debug, Clone)]
pub struct ConversationContext {
    pub session_id: Arc<str>,
    pub messages: Arc<[ChatMessage]>,
    /// Rendered run objective for this request, set only when
    /// `[agent.execution] run_objective` is `true`. It is request-scoped: sent
    /// in the request tail, never stored and never part of `messages`.
    pub request_objective: Option<Arc<str>>,
    /// Notices raised since the last model request. The loop stores them at the
    /// request boundary, after any tool results; until then they are not part
    /// of `messages`.
    pub pending_notices: Arc<[PendingNotice]>,
    /// Content of the most recent stored `agent_mode` notice in the effective
    /// history, `None` when there is none. A `ChatMessage` does not carry its
    /// notice source, so the mode comparison reads this instead.
    pub stored_mode_notice: Option<Arc<str>>,
    pub stats: Arc<AgentStats>,
    /// Current provider for the session (for dynamic model info lookup)
    pub provider: Arc<str>,
    /// Current model for the session (for dynamic model info lookup)
    pub model: Arc<str>,
    /// Per-session agent mode (Build, Plan, Review).
    /// Used by `AgentModeMiddleware` to inject mode-specific reminders.
    /// Defaults to `Build` for backward compatibility.
    pub session_mode: crate::agent::core::AgentMode,
    /// Generation overrides for the next model request only.
    ///
    /// Middleware sets these when it returns to `BeforeLlmCall` (e.g. to raise
    /// `max_tokens`). They are consumed by exactly one request: the context built
    /// after that request does not carry them forward.
    pub request_overrides: GenerationOptions,
    /// Facts about the session's model (caps, capabilities, effort, pricing).
    pub profile: Arc<ModelProfile>,
    /// Options the most recent model request was actually sent with.
    ///
    /// Carried to the `Stopped` state so `on_stopped` knows which cap was hit.
    /// Cleared at turn start.
    pub last_applied: GenerationOptions,
}

impl ConversationContext {
    pub fn new(
        session_id: Arc<str>,
        messages: Arc<[ChatMessage]>,
        stats: Arc<AgentStats>,
        provider: Arc<str>,
        model: Arc<str>,
    ) -> Self {
        Self {
            session_id,
            messages,
            request_objective: None,
            pending_notices: Arc::from([]),
            stored_mode_notice: None,
            stats,
            provider,
            model,
            session_mode: crate::agent::core::AgentMode::Build,
            request_overrides: GenerationOptions::NONE,
            profile: Arc::new(ModelProfile::default()),
            last_applied: GenerationOptions::NONE,
        }
    }

    /// Set the model profile (builder-style).
    #[must_use]
    pub fn with_profile(mut self, profile: Arc<ModelProfile>) -> Self {
        self.profile = profile;
        self
    }

    /// Set the options the last request was sent with (builder-style).
    #[must_use]
    pub fn with_last_applied(mut self, applied: GenerationOptions) -> Self {
        self.last_applied = applied;
        self
    }

    /// Returns the number of user messages in the conversation
    pub fn user_message_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|msg| matches!(msg.role, ChatRole::User))
            .count()
    }

    /// Queue a notice to be stored before the next model request.
    #[must_use]
    pub fn queue_notice(&self, source: NoticeSource, content: String) -> Self {
        let mut pending = self.pending_notices.to_vec();
        pending.push(PendingNotice { source, content });
        let mut next = self.clone();
        next.pending_notices = Arc::from(pending.into_boxed_slice());
        next
    }

    /// Carry queued notices and the stored mode notice over from `previous`
    /// to a rebuilt context (builder-style).
    #[must_use]
    pub fn carrying_notices_from(mut self, previous: &ConversationContext) -> Self {
        self.pending_notices = previous.pending_notices.clone();
        self.stored_mode_notice = previous.stored_mode_notice.clone();
        self
    }

    /// Set the stored mode notice (builder-style).
    #[must_use]
    pub fn with_stored_mode_notice(mut self, content: Option<Arc<str>>) -> Self {
        self.stored_mode_notice = content;
        self
    }

    /// Content of the most recent `agent_mode` notice, counting notices that
    /// are queued but not yet stored.
    pub fn last_mode_notice(&self) -> Option<&str> {
        self.pending_notices
            .iter()
            .rev()
            .find(|notice| notice.source == NoticeSource::AgentMode)
            .map(|notice| notice.content.as_str())
            .or(self.stored_mode_notice.as_deref())
    }

    /// Set the request-scoped run objective (builder-style).
    #[must_use]
    pub fn with_request_objective(mut self, objective: Option<Arc<str>>) -> Self {
        self.request_objective = objective;
        self
    }

    /// Set the per-session mode on this context (builder-style).
    pub fn with_session_mode(mut self, mode: crate::agent::core::AgentMode) -> Self {
        self.session_mode = mode;
        self
    }

    /// Set the overrides applied to the next model request (builder-style).
    #[must_use]
    pub fn with_request_overrides(mut self, overrides: GenerationOptions) -> Self {
        self.request_overrides = overrides;
        self
    }

    /// Returns a copy of this context whose overrides are cleared, so a
    /// consumed override never leaks into a later request.
    #[must_use]
    pub fn without_request_overrides(&self) -> Self {
        self.clone().with_request_overrides(GenerationOptions::NONE)
    }

    /// Returns true if this is the first user turn
    pub fn is_first_turn(&self) -> bool {
        self.stats.turns <= 1
    }
}

/// Response from the LLM provider
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    /// Accumulated thinking/reasoning content from streaming, if any.
    pub thinking: Option<String>,
    /// Signature for the accumulated thinking block, if provided by the model.
    pub thinking_signature: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<querymt::Usage>,
    pub finish_reason: Option<FinishReason>,
    /// Canonical ordered output items from the generation attempt.
    ///
    /// This is the source of truth for item-aware turns; `content`, `thinking`,
    /// and `tool_calls` are compatibility projections of it.
    pub output: Option<ChatOutput>,
    /// Pre-allocated message UUID from the streaming path.
    /// When set, `transition_after_llm` will use this ID so the final
    /// `AssistantMessageStored` event matches the delta events already sent to the UI.
    pub message_id: Option<String>,
    /// Per-request overrides this response's request was actually sent with
    /// (after dropping the ones the provider does not honour).
    pub applied: GenerationOptions,
}

impl LlmResponse {
    pub fn new(
        content: String,
        tool_calls: Vec<ToolCall>,
        usage: Option<querymt::Usage>,
        finish_reason: Option<FinishReason>,
    ) -> Self {
        Self {
            content,
            thinking: None,
            thinking_signature: None,
            tool_calls,
            usage,
            finish_reason,
            output: None,
            message_id: None,
            applied: GenerationOptions::NONE,
        }
    }

    /// Record the options the request was sent with (builder-style).
    pub fn with_applied(mut self, applied: GenerationOptions) -> Self {
        self.applied = applied;
        self
    }

    /// Set the canonical structured output (builder-style).
    pub fn with_output(mut self, output: Option<ChatOutput>) -> Self {
        self.output = output;
        self
    }

    /// Set the pre-allocated message ID (builder-style).
    pub fn with_message_id(mut self, id: String) -> Self {
        self.message_id = Some(id);
        self
    }

    /// Set the accumulated thinking content (builder-style).
    pub fn with_thinking(mut self, thinking: Option<String>) -> Self {
        self.thinking = thinking;
        self
    }

    /// Set the thinking signature (builder-style).
    pub fn with_thinking_signature(mut self, signature: Option<String>) -> Self {
        self.thinking_signature = signature;
        self
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    pub fn is_stop(&self) -> bool {
        self.finish_reason.unwrap_or(FinishReason::Unknown) == FinishReason::Stop
    }

    /// A reply cut off by the output token limit, whatever output status the
    /// provider reported. Providers that flush interrupted tool blocks as
    /// completed (invalid) calls still report `Completed` here.
    pub fn is_length_cutoff(&self) -> bool {
        self.finish_reason == Some(FinishReason::Length)
    }

    /// A reply cut off by the output token limit that produced no visible text
    /// and no tool calls, only reasoning (or nothing at all).
    ///
    /// Such a reply carries nothing worth keeping in history, and replaying it
    /// would only burn the next request's budget on the same reasoning.
    pub fn is_thinking_only_length_cutoff(&self) -> bool {
        self.finish_reason == Some(FinishReason::Length)
            && self.content.trim().is_empty()
            && self.tool_calls.is_empty()
            && self.output.as_ref().is_none_or(|output| {
                output
                    .items
                    .iter()
                    .all(|item| matches!(item, querymt::chat::ChatOutputItem::Reasoning(_)))
            })
    }
}

/// Result of a tool execution
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    pub content: Vec<querymt::chat::ToolResultPart>,
    pub is_error: bool,
    pub execution_is_error: bool,
    pub tool_source: String,
    pub tool_name: Option<String>,
    pub tool_arguments: Option<String>,
    pub hook_contexts: Vec<crate::hooks::HookContextContribution>,
    pub snapshot_part: Option<MessagePart>,
}

impl ToolResult {
    pub fn new(
        call_id: String,
        content: Vec<querymt::chat::ToolResultPart>,
        is_error: bool,
        tool_name: Option<String>,
        tool_arguments: Option<String>,
    ) -> Self {
        Self {
            call_id,
            content,
            is_error,
            execution_is_error: is_error,
            tool_source: "unknown".to_string(),
            tool_name,
            tool_arguments,
            hook_contexts: Vec::new(),
            snapshot_part: None,
        }
    }

    pub fn with_execution(mut self, is_error: bool, source: impl Into<String>) -> Self {
        self.execution_is_error = is_error;
        self.tool_source = source.into();
        self
    }

    pub fn with_hook_contexts(
        mut self,
        contexts: Vec<crate::hooks::HookContextContribution>,
    ) -> Self {
        self.hook_contexts = contexts;
        self
    }

    pub fn with_snapshot(mut self, part: MessagePart) -> Self {
        self.snapshot_part = Some(part);
        self
    }

    /// Extract concatenated text from content blocks for display purposes.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Describes why execution is waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    Delegation,
}

/// Condition describing when execution should resume.
#[derive(Debug, Clone)]
pub struct WaitCondition {
    pub reason: WaitReason,
    pub correlation_ids: Vec<String>,
}

impl WaitCondition {
    pub fn delegation(id: String) -> Self {
        Self {
            reason: WaitReason::Delegation,
            correlation_ids: vec![id],
        }
    }

    pub fn merge(mut conditions: Vec<WaitCondition>) -> Option<Self> {
        let mut reason: Option<WaitReason> = None;
        let mut correlation_ids = Vec::new();

        for condition in conditions.drain(..) {
            if reason.is_none() {
                reason = Some(condition.reason.clone());
            }
            if reason.as_ref() == Some(&condition.reason) {
                correlation_ids.extend(condition.correlation_ids);
            }
        }

        reason.map(|reason| Self {
            reason,
            correlation_ids,
        })
    }
}

/// Represents the complete state of agent execution
#[derive(Debug)]
pub enum ExecutionState {
    /// Before calling the LLM - middleware can inject messages or stop
    BeforeLlmCall { context: Arc<ConversationContext> },

    /// Ready to call the LLM with tools
    CallLlm {
        context: Arc<ConversationContext>,
        request: Arc<PreparedModelRequest>,
    },

    /// After receiving LLM response
    AfterLlm {
        response: Arc<LlmResponse>,
        context: Arc<ConversationContext>,
    },

    /// Processing multiple tool calls from a single LLM response
    ProcessingToolCalls {
        /// Remaining tool calls to process
        remaining_calls: Arc<[ToolCall]>,
        /// Tool results collected so far
        results: Arc<[ToolResult]>,
        /// Current conversation context
        context: Arc<ConversationContext>,
    },

    /// Waiting for an external event before continuing
    WaitingForEvent {
        context: Arc<ConversationContext>,
        wait: WaitCondition,
    },

    /// Execution completed successfully
    Complete { context: Arc<ConversationContext> },

    /// Execution stopped by middleware
    Stopped {
        message: Arc<str>,
        stop_type: StopType,
        context: Option<Arc<ConversationContext>>,
    },

    /// Execution cancelled by user
    Cancelled,
}

impl ExecutionState {
    /// Returns a human-readable name for the state
    pub fn name(&self) -> &'static str {
        match self {
            ExecutionState::BeforeLlmCall { .. } => "BeforeLlmCall",
            ExecutionState::CallLlm { .. } => "CallLlm",
            ExecutionState::AfterLlm { .. } => "AfterLlm",
            ExecutionState::ProcessingToolCalls { .. } => "ProcessingToolCalls",
            ExecutionState::WaitingForEvent { .. } => "WaitingForEvent",
            ExecutionState::Complete { .. } => "Complete",
            ExecutionState::Stopped { .. } => "Stopped",
            ExecutionState::Cancelled => "Cancelled",
        }
    }

    /// Returns the context if this state has one
    pub fn context(&self) -> Option<&Arc<ConversationContext>> {
        match self {
            ExecutionState::BeforeLlmCall { context } => Some(context),
            ExecutionState::CallLlm { context, .. } => Some(context),
            ExecutionState::AfterLlm { context, .. } => Some(context),
            ExecutionState::ProcessingToolCalls { context, .. } => Some(context),
            ExecutionState::WaitingForEvent { context, .. } => Some(context),
            ExecutionState::Complete { context } => Some(context),
            ExecutionState::Stopped { context, .. } => context.as_ref(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::ToolResultPart;

    fn context_with(messages: Vec<ChatMessage>) -> ConversationContext {
        ConversationContext::new(
            Arc::from("session"),
            Arc::from(messages),
            Arc::new(AgentStats::default()),
            Arc::from("provider"),
            Arc::from("model"),
        )
    }

    #[test]
    fn request_objective_is_not_part_of_the_durable_messages() {
        let latest = ChatMessage::user().text("Delegation completed").build();
        let context =
            context_with(vec![latest]).with_request_objective(Some(Arc::from("objective")));

        assert_eq!(context.messages.len(), 1);
        assert_eq!(context.messages[0].text(), "Delegation completed");
        assert_eq!(context.request_objective.as_deref(), Some("objective"));
    }

    #[test]
    fn contexts_carry_no_request_objective_by_default() {
        assert!(context_with(Vec::new()).request_objective.is_none());
    }

    #[test]
    fn test_tool_result_with_snapshot() {
        let result = ToolResult::new(
            "call-123".to_string(),
            vec![ToolResultPart::text("tool output")],
            false,
            Some("shell".to_string()),
            Some("{}".to_string()),
        );

        assert!(result.snapshot_part.is_none());

        let test_hash = querymt_agent_core::hash::RapidHash::new(b"test");
        let changed_paths = querymt_agent_index::merkle::DiffPaths {
            added: vec![],
            modified: vec![std::path::PathBuf::from("test.txt")],
            removed: vec![],
        };
        let result_with_snapshot = result.with_snapshot(MessagePart::Snapshot {
            root_hash: test_hash,
            changed_paths,
        });

        assert!(result_with_snapshot.snapshot_part.is_some());
        if let Some(MessagePart::Snapshot {
            root_hash,
            changed_paths,
        }) = result_with_snapshot.snapshot_part
        {
            assert_eq!(root_hash, test_hash);
            assert_eq!(changed_paths.summary(), "1 modified");
        }
    }

    #[test]
    fn test_calculate_context_tokens_with_reasoning() {
        // Test with all fields populated
        let usage = querymt::Usage {
            input_tokens: 100,
            output_tokens: 50,
            reasoning_tokens: 20,
            cache_read: 30,
            cache_write: 10,
            cache_write_tiers: None,
        };

        // Should sum all: 100 + 50 + 20 + 30 + 10 = 210
        assert_eq!(calculate_context_tokens(Some(&usage)), 210);
    }

    #[test]
    fn test_calculate_context_tokens_no_reasoning() {
        // Test without reasoning tokens
        let usage = querymt::Usage {
            input_tokens: 100,
            output_tokens: 50,
            reasoning_tokens: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write_tiers: None,
        };

        // Should sum: 100 + 50 + 0 + 0 + 0 = 150
        assert_eq!(calculate_context_tokens(Some(&usage)), 150);
    }

    #[test]
    fn test_calculate_context_tokens_with_cache() {
        // Test with cache but no reasoning
        let usage = querymt::Usage {
            input_tokens: 100,
            output_tokens: 50,
            reasoning_tokens: 0,
            cache_read: 30,
            cache_write: 10,
            cache_write_tiers: None,
        };

        // Should sum: 100 + 50 + 0 + 30 + 10 = 190
        assert_eq!(calculate_context_tokens(Some(&usage)), 190);
    }

    #[test]
    fn cache_write_tier_detail_is_not_added_to_context_total() {
        // Tiers partition `cache_write`; the aggregate is counted once.
        let usage = querymt::Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_write: 300,
            cache_write_tiers: Some(querymt::CacheWriteTiers {
                five_minute: Some(200),
                one_hour: Some(100),
            }),
            ..Default::default()
        };
        assert_eq!(calculate_context_tokens(Some(&usage)), 100 + 50 + 300);
    }

    #[test]
    fn test_calculate_context_tokens_none() {
        // Test with None
        assert_eq!(calculate_context_tokens(None), 0);
    }
}
