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
    },
    UserMessageStored {
        content: String,
    },
    AssistantMessageStored {
        content: String,
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
}

#[async_trait]
pub trait EventObserver: Send + Sync {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError>;
}
