use agent_client_protocol::StopReason;
use querymt::Usage;
use querymt::chat::FinishReason;
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EventOrigin {
    #[default]
    Local,
    Remote,
    Unknown(String),
}

impl Serialize for EventOrigin {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let value = match self {
            EventOrigin::Local => "local",
            EventOrigin::Remote => "remote",
            EventOrigin::Unknown(other) => other.as_str(),
        };
        serializer.serialize_str(value)
    }
}

impl<'de> Deserialize<'de> for EventOrigin {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "local" => EventOrigin::Local,
            "remote" => EventOrigin::Remote,
            _ => EventOrigin::Unknown(value),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEvent {
    pub seq: u64,
    pub timestamp: i64,
    pub session_id: String,
    #[serde(default)]
    pub origin: EventOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_node: Option<String>,
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
        thinking: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    /// Streaming text delta emitted per token/chunk during streaming.
    /// Ephemeral — must not be persisted to the event store.
    AssistantContentDelta {
        content: String,
        message_id: String,
    },
    /// Streaming thinking/reasoning delta emitted during streaming.
    /// Ephemeral — must not be persisted to the event store.
    AssistantThinkingDelta {
        content: String,
        message_id: String,
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
        /// Mesh node that owns this provider. `None` = local node.
        /// Included so the UI can display a node badge next to the model label.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_node_id: Option<String>,
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
    DelegationCancelRequested {
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
        /// Compacted list of duplicate code warnings (body_text stripped, matches capped)
        warnings: Vec<crate::middleware::dedup_check::DuplicateWarning>,
        /// Path to the full overflow report file (if written), readable via read_file
        overflow_path: Option<String>,
    },
    /// Emitted when the agent's operating mode changes at runtime
    ModeChanged {
        mode: String,
        previous_mode: String,
    },
    /// Emitted when a session's mode changes (per-session mode in actor model)
    SessionModeChanged {
        mode: crate::agent::core::AgentMode,
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
    /// Emitted on the remote node once its workspace index has finished
    /// building and is available via `GetFileIndex`.  Flows through the
    /// EventForwarder → EventRelayActor → local EventSink chain so the
    /// local UI server can react without polling.
    WorkspaceIndexReady {
        workspace_root: String,
    },
}

// ============================================================================
// Event Bus Refactor: Durability classification + Envelope types
// ============================================================================

/// Whether an event must be persisted to the journal (durable) or is
/// live-only (ephemeral).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    /// Must be persisted; replayable via journal.
    Durable,
    /// Live-only; never written to journal, never replayed.
    Ephemeral,
}

/// A durable event that has been persisted to the journal.
///
/// Contains a DB-assigned `event_id` and monotonic `stream_seq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableEvent {
    pub event_id: String,
    pub stream_seq: u64,
    pub session_id: String,
    pub timestamp: i64,
    pub origin: EventOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_node: Option<String>,
    pub kind: AgentEventKind,
}

impl From<DurableEvent> for AgentEvent {
    fn from(de: DurableEvent) -> Self {
        AgentEvent {
            seq: de.stream_seq,
            timestamp: de.timestamp,
            session_id: de.session_id,
            origin: de.origin,
            source_node: de.source_node,
            kind: de.kind,
        }
    }
}

/// An ephemeral event — live delivery only, no persistence, no sequence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EphemeralEvent {
    pub session_id: String,
    pub timestamp: i64,
    pub origin: EventOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_node: Option<String>,
    pub kind: AgentEventKind,
}

impl From<EphemeralEvent> for AgentEvent {
    fn from(ee: EphemeralEvent) -> Self {
        AgentEvent {
            seq: 0,
            timestamp: ee.timestamp,
            session_id: ee.session_id,
            origin: ee.origin,
            source_node: ee.source_node,
            kind: ee.kind,
        }
    }
}

impl From<AgentEvent> for EventEnvelope {
    fn from(event: AgentEvent) -> Self {
        EventEnvelope::Durable(DurableEvent {
            event_id: format!("legacy-{}", event.seq),
            stream_seq: event.seq,
            session_id: event.session_id,
            timestamp: event.timestamp,
            origin: event.origin,
            source_node: event.source_node,
            kind: event.kind,
        })
    }
}

/// A tagged union of durable and ephemeral events for the fanout layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "durability", rename_all = "snake_case")]
pub enum EventEnvelope {
    Durable(DurableEvent),
    Ephemeral(EphemeralEvent),
}

impl EventEnvelope {
    pub fn session_id(&self) -> &str {
        match self {
            EventEnvelope::Durable(e) => &e.session_id,
            EventEnvelope::Ephemeral(e) => &e.session_id,
        }
    }

    pub fn kind(&self) -> &AgentEventKind {
        match self {
            EventEnvelope::Durable(e) => &e.kind,
            EventEnvelope::Ephemeral(e) => &e.kind,
        }
    }

    pub fn timestamp(&self) -> i64 {
        match self {
            EventEnvelope::Durable(e) => e.timestamp,
            EventEnvelope::Ephemeral(e) => e.timestamp,
        }
    }

    pub fn is_durable(&self) -> bool {
        matches!(self, EventEnvelope::Durable(_))
    }

    pub fn is_ephemeral(&self) -> bool {
        matches!(self, EventEnvelope::Ephemeral(_))
    }

    /// Sequence number: `stream_seq` for durable events, `0` for ephemeral.
    pub fn seq(&self) -> u64 {
        match self {
            EventEnvelope::Durable(e) => e.stream_seq,
            EventEnvelope::Ephemeral(_) => 0,
        }
    }

    pub fn origin(&self) -> &EventOrigin {
        match self {
            EventEnvelope::Durable(e) => &e.origin,
            EventEnvelope::Ephemeral(e) => &e.origin,
        }
    }

    pub fn source_node(&self) -> Option<&str> {
        match self {
            EventEnvelope::Durable(e) => e.source_node.as_deref(),
            EventEnvelope::Ephemeral(e) => e.source_node.as_deref(),
        }
    }
}

/// Classify an `AgentEventKind` as durable or ephemeral.
///
/// **Policy**: durable by default. Only explicitly transient signal types
/// are ephemeral. Each ephemeral entry has a rationale comment.
pub fn classify_durability(kind: &AgentEventKind) -> Durability {
    match kind {
        // ── Ephemeral: high-frequency streaming deltas ──────────────
        // Rationale: per-token chunks; the full content is captured by
        // AssistantMessageStored which IS durable.
        AgentEventKind::AssistantContentDelta { .. } => Durability::Ephemeral,
        AgentEventKind::AssistantThinkingDelta { .. } => Durability::Ephemeral,

        // Everything else is durable by default.
        _ => Durability::Durable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── StopType -> StopReason conversion ──────────────────────────────────

    #[test]
    fn stop_type_step_limit_converts_to_max_turn_requests() {
        let stop_reason: StopReason = StopType::StepLimit.into();
        assert_eq!(stop_reason, StopReason::MaxTurnRequests);
    }

    #[test]
    fn stop_type_turn_limit_converts_to_max_turn_requests() {
        let stop_reason: StopReason = StopType::TurnLimit.into();
        assert_eq!(stop_reason, StopReason::MaxTurnRequests);
    }

    #[test]
    fn stop_type_delegation_blocked_converts_to_max_turn_requests() {
        let stop_reason: StopReason = StopType::DelegationBlocked.into();
        assert_eq!(stop_reason, StopReason::MaxTurnRequests);
    }

    #[test]
    fn stop_type_price_limit_converts_to_max_tokens() {
        let stop_reason: StopReason = StopType::PriceLimit.into();
        assert_eq!(stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn stop_type_context_threshold_converts_to_max_tokens() {
        let stop_reason: StopReason = StopType::ContextThreshold.into();
        assert_eq!(stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn stop_type_model_token_limit_converts_to_max_tokens() {
        let stop_reason: StopReason = StopType::ModelTokenLimit.into();
        assert_eq!(stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn stop_type_content_filter_converts_to_end_turn() {
        let stop_reason: StopReason = StopType::ContentFilter.into();
        assert_eq!(stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn stop_type_other_converts_to_end_turn() {
        let stop_reason: StopReason = StopType::Other.into();
        assert_eq!(stop_reason, StopReason::EndTurn);
    }

    // ── StopType serialization round-trip ──────────────────────────────────

    #[test]
    fn stop_type_serializes_as_snake_case() {
        let stop_type = StopType::StepLimit;
        let json = serde_json::to_string(&stop_type).unwrap();
        assert_eq!(json, r#""step_limit""#);
    }

    #[test]
    fn stop_type_deserializes_from_snake_case() {
        let json = r#""turn_limit""#;
        let stop_type: StopType = serde_json::from_str(json).unwrap();
        assert_eq!(stop_type, StopType::TurnLimit);
    }

    #[test]
    fn all_stop_type_variants_round_trip() {
        let variants = vec![
            StopType::StepLimit,
            StopType::TurnLimit,
            StopType::PriceLimit,
            StopType::ContextThreshold,
            StopType::ModelTokenLimit,
            StopType::ContentFilter,
            StopType::DelegationBlocked,
            StopType::Other,
        ];

        for original in variants {
            let json = serde_json::to_string(&original).unwrap();
            let restored: StopType = serde_json::from_str(&json).unwrap();
            assert_eq!(original, restored);
        }
    }

    // ── AgentEventKind variant construction ────────────────────────────────

    #[test]
    fn agent_event_kind_session_created_construction() {
        let kind = AgentEventKind::SessionCreated;
        let json = serde_json::to_value(&kind).unwrap();
        assert_eq!(json["type"], "session_created");
    }

    #[test]
    fn agent_event_kind_prompt_received_construction() {
        let kind = AgentEventKind::PromptReceived {
            content: "test prompt".to_string(),
            message_id: Some("msg-1".to_string()),
        };
        let json = serde_json::to_value(&kind).unwrap();
        assert_eq!(json["type"], "prompt_received");
        assert_eq!(json["content"], "test prompt");
        assert_eq!(json["message_id"], "msg-1");
    }

    #[test]
    fn agent_event_kind_middleware_stopped_construction() {
        let kind = AgentEventKind::MiddlewareStopped {
            stop_type: StopType::StepLimit,
            reason: "max steps reached".to_string(),
            metrics: ExecutionMetrics {
                steps: 10,
                turns: 5,
            },
        };
        let json = serde_json::to_value(&kind).unwrap();
        assert_eq!(json["type"], "middleware_stopped");
        assert_eq!(json["stop_type"], "step_limit");
        assert_eq!(json["reason"], "max steps reached");
        assert_eq!(json["metrics"]["steps"], 10);
        assert_eq!(json["metrics"]["turns"], 5);
    }

    #[test]
    fn agent_event_kind_error_construction() {
        let kind = AgentEventKind::Error {
            message: "test error".to_string(),
        };
        let json = serde_json::to_value(&kind).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["message"], "test error");
    }

    // ── EventOrigin serialization/compatibility ────────────────────────────

    #[test]
    fn event_origin_deserializes_unknown_value_to_unknown_variant() {
        let origin: EventOrigin = serde_json::from_str("\"mesh_bridge\"").unwrap();
        assert_eq!(origin, EventOrigin::Unknown("mesh_bridge".to_string()));
    }

    #[test]
    fn event_origin_serializes_unknown_value_verbatim() {
        let origin = EventOrigin::Unknown("mesh_bridge".to_string());
        let json = serde_json::to_string(&origin).unwrap();
        assert_eq!(json, "\"mesh_bridge\"");
    }

    // ── AgentEvent clone + debug ───────────────────────────────────────────

    #[test]
    fn agent_event_implements_clone() {
        let original = AgentEvent {
            seq: 1,
            timestamp: 1234567890,
            session_id: "sess-1".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };
        let cloned = original.clone();
        assert_eq!(cloned.seq, 1);
        assert_eq!(cloned.session_id, "sess-1");
    }

    #[test]
    fn agent_event_implements_debug() {
        let event = AgentEvent {
            seq: 42,
            timestamp: 1234567890,
            session_id: "sess-debug".to_string(),
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::Cancelled,
        };
        let debug_str = format!("{:?}", event);
        assert!(debug_str.contains("seq: 42"));
        assert!(debug_str.contains("sess-debug"));
        assert!(debug_str.contains("Cancelled"));
    }

    // ── ExecutionMetrics defaults and serialization ────────────────────────

    #[test]
    fn execution_metrics_default_is_zero() {
        let metrics = ExecutionMetrics::default();
        assert_eq!(metrics.steps, 0);
        assert_eq!(metrics.turns, 0);
    }

    #[test]
    fn execution_metrics_serializes_correctly() {
        let metrics = ExecutionMetrics { steps: 5, turns: 3 };
        let json = serde_json::to_value(&metrics).unwrap();
        assert_eq!(json["steps"], 5);
        assert_eq!(json["turns"], 3);
    }

    // ── SessionLimits defaults and serialization ───────────────────────────

    #[test]
    fn session_limits_default_has_all_none() {
        let limits = SessionLimits::default();
        assert!(limits.max_steps.is_none());
        assert!(limits.max_turns.is_none());
        assert!(limits.max_cost_usd.is_none());
    }

    #[test]
    fn session_limits_serializes_correctly() {
        let limits = SessionLimits {
            max_steps: Some(100),
            max_turns: Some(50),
            max_cost_usd: Some(1.5),
        };
        let json = serde_json::to_value(&limits).unwrap();
        assert_eq!(json["max_steps"], 100);
        assert_eq!(json["max_turns"], 50);
        assert_eq!(json["max_cost_usd"], 1.5);
    }

    // ── Durability classification ──────────────────────────────────────────

    #[test]
    fn durability_serde_round_trip() {
        let durable = Durability::Durable;
        let json = serde_json::to_string(&durable).unwrap();
        assert_eq!(json, "\"durable\"");
        let restored: Durability = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, Durability::Durable);

        let ephemeral = Durability::Ephemeral;
        let json = serde_json::to_string(&ephemeral).unwrap();
        assert_eq!(json, "\"ephemeral\"");
        let restored: Durability = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, Durability::Ephemeral);
    }

    #[test]
    fn classify_content_delta_is_ephemeral() {
        let kind = AgentEventKind::AssistantContentDelta {
            content: "hello".into(),
            message_id: "m1".into(),
        };
        assert_eq!(classify_durability(&kind), Durability::Ephemeral);
    }

    #[test]
    fn classify_thinking_delta_is_ephemeral() {
        let kind = AgentEventKind::AssistantThinkingDelta {
            content: "hmm".into(),
            message_id: "m1".into(),
        };
        assert_eq!(classify_durability(&kind), Durability::Ephemeral);
    }

    #[test]
    fn classify_session_created_is_durable() {
        assert_eq!(
            classify_durability(&AgentEventKind::SessionCreated),
            Durability::Durable
        );
    }

    #[test]
    fn classify_cancelled_is_durable() {
        assert_eq!(
            classify_durability(&AgentEventKind::Cancelled),
            Durability::Durable
        );
    }

    #[test]
    fn classify_error_is_durable() {
        let kind = AgentEventKind::Error {
            message: "fail".into(),
        };
        assert_eq!(classify_durability(&kind), Durability::Durable);
    }

    #[test]
    fn classify_assistant_message_stored_is_durable() {
        let kind = AgentEventKind::AssistantMessageStored {
            content: "hi".into(),
            thinking: None,
            message_id: None,
        };
        assert_eq!(classify_durability(&kind), Durability::Durable);
    }

    #[test]
    fn classify_tool_call_start_is_durable() {
        let kind = AgentEventKind::ToolCallStart {
            tool_call_id: "tc1".into(),
            tool_name: "shell".into(),
            arguments: "{}".into(),
        };
        assert_eq!(classify_durability(&kind), Durability::Durable);
    }

    #[test]
    fn classify_elicitation_requested_is_durable() {
        let kind = AgentEventKind::ElicitationRequested {
            elicitation_id: "e1".into(),
            session_id: "s1".into(),
            message: "what?".into(),
            requested_schema: serde_json::json!({}),
            source: "test".into(),
        };
        assert_eq!(classify_durability(&kind), Durability::Durable);
    }

    // ── DurableEvent construction ──────────────────────────────────────────

    #[test]
    fn durable_event_construction_and_fields() {
        let event = DurableEvent {
            event_id: "evt-1".into(),
            stream_seq: 42,
            session_id: "sess-1".into(),
            timestamp: 1700000000,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        };
        assert_eq!(event.stream_seq, 42);
        assert_eq!(event.event_id, "evt-1");
        assert_eq!(event.session_id, "sess-1");
    }

    #[test]
    fn durable_event_serde_round_trip() {
        let event = DurableEvent {
            event_id: "evt-2".into(),
            stream_seq: 7,
            session_id: "sess-2".into(),
            timestamp: 1700000001,
            origin: EventOrigin::Remote,
            source_node: Some("node-a".into()),
            kind: AgentEventKind::Cancelled,
        };
        let json = serde_json::to_string(&event).unwrap();
        let restored: DurableEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.event_id, "evt-2");
        assert_eq!(restored.stream_seq, 7);
        assert!(matches!(restored.origin, EventOrigin::Remote));
        assert_eq!(restored.source_node.as_deref(), Some("node-a"));
    }

    // ── EphemeralEvent construction ────────────────────────────────────────

    #[test]
    fn ephemeral_event_construction() {
        let event = EphemeralEvent {
            session_id: "sess-e".into(),
            timestamp: 123,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "tok".into(),
                message_id: "m1".into(),
            },
        };
        assert_eq!(event.session_id, "sess-e");
    }

    // ── EventEnvelope ──────────────────────────────────────────────────────

    #[test]
    fn envelope_durable_accessors() {
        let env = EventEnvelope::Durable(DurableEvent {
            event_id: "e1".into(),
            stream_seq: 1,
            session_id: "s1".into(),
            timestamp: 100,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::SessionCreated,
        });
        assert_eq!(env.session_id(), "s1");
        assert_eq!(env.timestamp(), 100);
        assert!(env.is_durable());
        assert!(!env.is_ephemeral());
        assert!(matches!(env.kind(), AgentEventKind::SessionCreated));
    }

    #[test]
    fn envelope_ephemeral_accessors() {
        let env = EventEnvelope::Ephemeral(EphemeralEvent {
            session_id: "s2".into(),
            timestamp: 200,
            origin: EventOrigin::Local,
            source_node: None,
            kind: AgentEventKind::AssistantContentDelta {
                content: "x".into(),
                message_id: "m".into(),
            },
        });
        assert_eq!(env.session_id(), "s2");
        assert!(!env.is_durable());
        assert!(env.is_ephemeral());
    }
}
