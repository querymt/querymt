use agent_client_protocol::StopReason;
use async_trait::async_trait;
use querymt::Usage;
use querymt::chat::FinishReason;
use querymt::error::LLMError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::McpServerConfig;
use crate::hash::RapidHash;
use crate::session::domain::{
    Alternative, Artifact, Decision, Delegation, ForkOrigin, ForkPointType, IntentSnapshot,
    ProgressEntry, Task,
};

/// Why execution was stopped
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopType {
    /// Step limit reached (max LLM calls)
    StepLimit,
    /// Turn limit reached (max user/assistant exchanges)
    TurnLimit,
    /// Price/cost limit exceeded
    PriceLimit,
    /// Context token threshold reached (compaction needed)
    ContextThreshold,
    /// Model hit its token limit
    ModelTokenLimit,
    /// Content filter blocked the response
    ContentFilter,
    /// Delegation was blocked
    DelegationBlocked,
    /// Generic/unknown stop reason
    Other,
}

impl From<StopType> for StopReason {
    fn from(stop_type: StopType) -> Self {
        match stop_type {
            StopType::StepLimit | StopType::TurnLimit | StopType::DelegationBlocked => {
                StopReason::MaxTurnRequests
            }
            StopType::PriceLimit | StopType::ContextThreshold | StopType::ModelTokenLimit => {
                StopReason::MaxTokens
            }
            StopType::ContentFilter | StopType::Other => StopReason::EndTurn,
        }
    }
}

/// Execution progress metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecutionMetrics {
    /// Number of LLM calls made
    pub steps: usize,
    /// Number of user/assistant turns
    pub turns: usize,
}

/// Session limits configuration (exposed to UI)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionLimits {
    /// Maximum number of LLM calls
    pub max_steps: Option<usize>,
    /// Maximum number of user/assistant turns
    pub max_turns: Option<usize>,
    /// Maximum cost in USD
    pub max_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub seq: u64,
    pub timestamp: i64,
    pub session_id: String,
    pub kind: AgentEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEventKind {
    SessionCreated,
    PromptReceived {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    UserMessageStored {
        content: String,
    },
    AssistantMessageStored {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    LlmRequestStart {
        message_count: usize,
    },
    LlmRequestEnd {
        usage: Option<Usage>,
        tool_calls: usize,
        finish_reason: Option<FinishReason>,
        /// Cost information for this request in USD
        cost_usd: Option<f64>,
        /// Cumulative cost for the session in USD
        cumulative_cost_usd: Option<f64>,
        /// Current context size (input + output tokens)
        context_tokens: u64,
        /// Execution progress metrics (steps/turns)
        metrics: ExecutionMetrics,
    },
    ProviderChanged {
        provider: String,
        model: String,
        config_id: i64,
        context_limit: Option<u64>,
    },
    ToolCallStart {
        tool_call_id: String,
        tool_name: String,
        arguments: String,
    },
    ToolCallEnd {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        result: String,
    },
    SnapshotStart {
        policy: String,
    },
    SnapshotEnd {
        summary: Option<String>,
    },
    CompactionStart {
        token_estimate: usize,
    },
    CompactionEnd {
        summary: String,
        summary_len: usize,
    },
    MiddlewareInjected {
        message: String,
    },
    MiddlewareStopped {
        /// Type of stop (for UI to handle differently)
        stop_type: StopType,
        /// Human-readable reason message
        reason: String,
        /// Execution metrics at time of stop
        metrics: ExecutionMetrics,
    },
    /// Emitted when a prompt is queued because another operation is executing
    SessionQueued {
        /// Reason for queueing (e.g., "waiting for previous operation to complete")
        reason: String,
    },
    Cancelled,
    Error {
        message: String,
    },
    // Phase 2: New event types for structured domain tracking
    // Domain events embed full domain structs for completeness
    IntentUpdated {
        intent_snapshot: IntentSnapshot,
    },
    TaskCreated {
        task: Task,
    },
    TaskUpdated {
        task: Task,
    },
    TaskStatusChanged {
        task: Task,
    },
    DecisionRecorded {
        decision: Decision,
    },
    AlternativeRecorded {
        alternative: Alternative,
    },
    AlternativeDiscarded {
        alternative_id: i64,
        task_id: Option<i64>,
    },
    ProgressRecorded {
        progress_entry: ProgressEntry,
    },
    ArtifactRecorded {
        artifact: Artifact,
    },
    DelegationRequested {
        delegation: Delegation,
    },
    DelegationCompleted {
        delegation_id: String,
        result: Option<String>,
    },
    DelegationFailed {
        delegation_id: String,
        error: String,
    },
    DelegationCancelled {
        delegation_id: String,
    },
    UncertaintyEscalated {
        task_id: Option<String>,
        description: String,
        options: Vec<String>,
    },
    PermissionRequested {
        permission_id: String,
        task_id: Option<String>,
        tool_name: String,
        reason: String,
    },
    PermissionGranted {
        permission_id: String,
        granted: bool,
    },
    ElicitationRequested {
        elicitation_id: String,
        session_id: String,
        message: String,
        requested_schema: serde_json::Value,
        source: String,
    },
    SessionForked {
        parent_session_id: String,
        child_session_id: String,
        target_agent_id: String,
        origin: ForkOrigin,
        fork_point_type: ForkPointType,
        fork_point_ref: String,
        instructions: Option<String>,
    },
    /// Emitted once at session creation with environment configuration
    SessionConfigured {
        cwd: Option<PathBuf>,
        mcp_servers: Vec<McpServerConfig>,
        /// Session limits configuration (if any)
        limits: Option<SessionLimits>,
    },
    /// Emitted at session start and whenever available tools change
    ToolsAvailable {
        tools: Vec<querymt::chat::Tool>,
        tools_hash: RapidHash,
    },
    /// Emitted when duplicate/similar code is detected in newly written code
    DuplicateCodeDetected {
        /// List of duplicate code warnings
        warnings: Vec<crate::middleware::dedup_check::DuplicateWarning>,
    },
    /// Emitted when the agent's operating mode changes at runtime
    ModeChanged {
        mode: String,
        previous_mode: String,
    },
    /// LLM request was rate limited, execution is paused and waiting
    RateLimited {
        /// Human-readable message from the provider
        message: String,
        /// Seconds until retry will be attempted
        wait_secs: u64,
        /// When the wait started (Unix timestamp in seconds)
        started_at: i64,
        /// Current retry attempt (1-indexed)
        attempt: usize,
        /// Maximum retry attempts configured
        max_attempts: usize,
    },
    /// Rate limit wait completed, resuming execution
    RateLimitResume {
        /// Which attempt is now being made
        attempt: usize,
    },
}

#[async_trait]
pub trait EventObserver: Send + Sync {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError>;
}
