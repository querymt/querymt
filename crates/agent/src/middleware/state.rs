use agent_client_protocol::StopReason;
use querymt::chat::{ChatMessage, ChatRole, FinishReason};
use std::sync::Arc;

use crate::model::MessagePart;

/// Statistics about agent execution
#[derive(Debug, Clone, Default)]
pub struct AgentStats {
    pub steps: usize,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub context_tokens: usize,
    /// Total cost in USD calculated from token usage and pricing
    pub total_cost_usd: f64,
    /// Input cost in USD
    pub input_cost_usd: f64,
    /// Output cost in USD
    pub output_cost_usd: f64,
    /// Cache read tokens (for providers that support prompt caching)
    pub cache_read_tokens: u64,
    /// Cache write tokens (for providers that support prompt caching)
    pub cache_write_tokens: u64,
    /// Cache read cost in USD
    pub cache_read_cost_usd: f64,
    /// Cache write cost in USD
    pub cache_write_cost_usd: f64,
}

impl AgentStats {
    /// Update costs based on pricing information
    ///
    /// This should be called whenever token counts are updated
    pub fn update_costs(&mut self, pricing: &querymt::providers::ModelPricing) {
        // Use ModelPricing methods directly

        // Calculate base token costs
        if let Some(cost) =
            pricing.calculate_cost(self.total_input_tokens, self.total_output_tokens)
        {
            self.total_cost_usd = cost;

            // Calculate individual components if pricing is available
            if let Some(input_rate) = pricing.input {
                self.input_cost_usd = (self.total_input_tokens as f64 / 1_000_000.0) * input_rate;
            }
            if let Some(output_rate) = pricing.output {
                self.output_cost_usd =
                    (self.total_output_tokens as f64 / 1_000_000.0) * output_rate;
            }
        }

        // Calculate cache costs if available using ModelPricing method
        let (cache_read_cost, cache_write_cost) =
            pricing.calculate_cache_cost(self.cache_read_tokens, self.cache_write_tokens);

        if let Some(read_cost) = cache_read_cost {
            self.cache_read_cost_usd = read_cost;
            self.total_cost_usd += read_cost;
        }

        if let Some(write_cost) = cache_write_cost {
            self.cache_write_cost_usd = write_cost;
            self.total_cost_usd += write_cost;
        }
    }
}

/// Token usage information
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl TokenUsage {
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
        }
    }
}

/// Represents a tool call
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: String,
}

/// Context passed to middleware during state transitions
#[derive(Debug, Clone)]
pub struct ConversationContext {
    pub session_id: Arc<str>,
    pub messages: Arc<[ChatMessage]>,
    pub stats: Arc<AgentStats>,
    /// Current provider for the session (for dynamic model info lookup)
    pub provider: Arc<str>,
    /// Current model for the session (for dynamic model info lookup)
    pub model: Arc<str>,
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
            stats,
            provider,
            model,
        }
    }

    /// Returns the number of user messages in the conversation
    pub fn user_message_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|msg| matches!(msg.role, ChatRole::User))
            .count()
    }

    /// Creates a new context with an injected message added
    pub fn inject_message(&self, content: String) -> Self {
        let mut messages = Vec::from(&*self.messages);

        let injected_msg = ChatMessage {
            role: ChatRole::User,
            message_type: querymt::chat::MessageType::Text,
            content,
        };

        messages.push(injected_msg);

        Self {
            session_id: self.session_id.clone(),
            messages: Arc::from(messages.into_boxed_slice()),
            stats: self.stats.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }

    /// Returns true if this is the first user turn
    pub fn is_first_turn(&self) -> bool {
        self.user_message_count() == 1
    }
}

/// Response from the LLM provider
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<TokenUsage>,
    pub finish_reason: Option<FinishReason>,
}

impl LlmResponse {
    pub fn new(
        content: String,
        tool_calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
        finish_reason: Option<FinishReason>,
    ) -> Self {
        Self {
            content,
            tool_calls,
            usage,
            finish_reason,
        }
    }

    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    pub fn is_stop(&self) -> bool {
        self.finish_reason.unwrap_or(FinishReason::Unknown) == FinishReason::Stop
    }
}

/// Result of a tool execution
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
    pub tool_name: Option<String>,
    pub tool_arguments: Option<String>,
    pub snapshot_part: Option<MessagePart>,
}

impl ToolResult {
    pub fn new(
        call_id: String,
        content: String,
        is_error: bool,
        tool_name: Option<String>,
        tool_arguments: Option<String>,
    ) -> Self {
        Self {
            call_id,
            content,
            is_error,
            tool_name,
            tool_arguments,
            snapshot_part: None,
        }
    }

    pub fn with_snapshot(mut self, part: MessagePart) -> Self {
        self.snapshot_part = Some(part);
        self
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
    BeforeTurn { context: Arc<ConversationContext> },

    /// Ready to call the LLM with tools
    CallLlm {
        context: Arc<ConversationContext>,
        tools: Arc<[querymt::chat::Tool]>,
    },

    /// After receiving LLM response
    AfterLlm {
        response: Arc<LlmResponse>,
        context: Arc<ConversationContext>,
    },

    /// Before executing a tool call - middleware can block or modify
    BeforeToolCall {
        call: Arc<ToolCall>,
        context: Arc<ConversationContext>,
    },

    /// After tool execution completed
    AfterTool {
        result: Arc<ToolResult>,
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
    Complete,

    /// Execution stopped by middleware
    Stopped {
        reason: StopReason,
        message: Arc<str>,
    },

    /// Execution cancelled by user
    Cancelled,
}

impl ExecutionState {
    /// Returns a human-readable name for the state
    pub fn name(&self) -> &'static str {
        match self {
            ExecutionState::BeforeTurn { .. } => "BeforeTurn",
            ExecutionState::CallLlm { .. } => "CallLlm",
            ExecutionState::AfterLlm { .. } => "AfterLlm",
            ExecutionState::BeforeToolCall { .. } => "BeforeToolCall",
            ExecutionState::AfterTool { .. } => "AfterTool",
            ExecutionState::ProcessingToolCalls { .. } => "ProcessingToolCalls",
            ExecutionState::WaitingForEvent { .. } => "WaitingForEvent",
            ExecutionState::Complete => "Complete",
            ExecutionState::Stopped { .. } => "Stopped",
            ExecutionState::Cancelled => "Cancelled",
        }
    }

    /// Returns the context if this state has one
    pub fn context(&self) -> Option<&Arc<ConversationContext>> {
        match self {
            ExecutionState::BeforeTurn { context } => Some(context),
            ExecutionState::CallLlm { context, .. } => Some(context),
            ExecutionState::AfterLlm { context, .. } => Some(context),
            ExecutionState::BeforeToolCall { context, .. } => Some(context),
            ExecutionState::AfterTool { context, .. } => Some(context),
            ExecutionState::ProcessingToolCalls { context, .. } => Some(context),
            ExecutionState::WaitingForEvent { context, .. } => Some(context),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_result_with_snapshot() {
        let result = ToolResult::new(
            "call-123".to_string(),
            "tool output".to_string(),
            false,
            Some("shell".to_string()),
            Some("{}".to_string()),
        );

        assert!(result.snapshot_part.is_none());

        let test_hash = crate::hash::RapidHash::new(b"test");
        let changed_paths = crate::index::merkle::DiffPaths {
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
}
