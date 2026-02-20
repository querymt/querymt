//! Defines the generic storage interface for session persistence.

use crate::config::{CompactionConfig, PruningConfig, RateLimitConfig, ToolOutputConfig};
use crate::model::AgentMessage;
use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, ForkPointType, IntentSnapshot, ProgressEntry,
    ProgressKind, Task, TaskStatus,
};
use crate::session::error::{SessionError, SessionResult};
use async_trait::async_trait;
use querymt::LLMParams;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

/// Represents the metadata for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    #[serde(skip)]
    pub id: i64,
    pub public_id: String,
    pub name: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Reference to current intent snapshot (internal ID)
    #[serde(skip)]
    pub current_intent_snapshot_id: Option<i64>,
    /// Active task for this session (internal ID)
    #[serde(skip)]
    pub active_task_id: Option<i64>,
    /// Current LLM configuration for this session (internal ID)
    #[serde(skip)]
    pub llm_config_id: Option<i64>,
    /// Parent session if this is a fork (internal ID)
    #[serde(skip)]
    pub parent_session_id: Option<i64>,
    /// Fork origin (user or delegation)
    pub fork_origin: Option<ForkOrigin>,
    /// Fork point type (message_index or progress_entry)
    pub fork_point_type: Option<ForkPointType>,
    /// Fork point reference (message ID or progress entry ID)
    pub fork_point_ref: Option<String>,
    /// Instructions provided when forking
    pub fork_instructions: Option<String>,
}

/// Stored LLM configuration (internal only, no public_id needed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMConfig {
    #[serde(skip)]
    pub id: i64,
    pub name: Option<String>,
    pub provider: String,
    pub model: String,
    pub params: Option<serde_json::Value>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Mesh node label that owns this provider. `None` = local node.
    /// Stored as `"_node"` key in the `params` JSON blob so no schema migration is needed.
    /// The leading underscore signals it is an internal routing field, not a provider param.
    #[serde(skip)]
    pub provider_node: Option<String>,
}

/// Stored session-scoped execution configuration snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExecutionConfig {
    pub max_steps: Option<usize>,
    pub max_prompt_bytes: Option<usize>,
    pub execution_timeout_secs: u64,
    pub snapshot_policy: String,
    pub tool_output_config: ToolOutputConfig,
    pub pruning_config: PruningConfig,
    pub compaction_config: CompactionConfig,
    pub rate_limit_config: RateLimitConfig,
}

impl Default for SessionExecutionConfig {
    fn default() -> Self {
        Self {
            max_steps: None,
            max_prompt_bytes: None,
            execution_timeout_secs: 300,
            snapshot_policy: "none".to_string(),
            tool_output_config: ToolOutputConfig::default(),
            pruning_config: PruningConfig::default(),
            compaction_config: CompactionConfig::default(),
            rate_limit_config: RateLimitConfig::default(),
        }
    }
}

/// Helper to extract config values from LLMParams for storage
pub(crate) fn extract_llm_config_values(
    params: &LLMParams,
) -> Result<(String, String, Option<Value>), SessionError> {
    let provider = params
        .provider
        .as_ref()
        .ok_or_else(|| SessionError::InvalidOperation("Provider is required".to_string()))?
        .clone();

    let model = params
        .model
        .as_ref()
        .ok_or_else(|| SessionError::InvalidOperation("Model is required".to_string()))?
        .clone();

    // Serialize to JSON for storage, but exclude provider, model, and name
    // since those are stored separately in dedicated columns
    let params_json = serde_json::to_value(params).map_err(|e| {
        SessionError::InvalidOperation(format!("Failed to serialize params: {}", e))
    })?;

    // Filter out fields that are stored separately or should not be persisted
    // - provider, model, name: stored in dedicated columns
    // - api_key: sensitive credential, should come from env vars at runtime
    let filtered_params = if let Some(obj) = params_json.as_object() {
        let filtered: serde_json::Map<String, Value> = obj
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "provider" | "model" | "name" | "api_key"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if filtered.is_empty() {
            None
        } else {
            Some(Value::Object(filtered))
        }
    } else {
        None
    };

    Ok((provider, model, filtered_params))
}

/// A generic, asynchronous trait for storing and retrieving chat sessions.
///
/// ## Thread Safety & Session Isolation
///
/// Implementations of this trait MUST guarantee strict session isolation:
/// - Each session is identified by a unique session_id
/// - Operations on one session MUST NOT affect other sessions
/// - Multiple concurrent operations on different sessions MUST NOT block each other
/// - The trait is `Send + Sync` to enable parallel session handling
///
/// ## Concurrency Model
///
/// The `SessionStore` trait is designed to support high concurrency:
/// - Multiple clients can interact with different sessions simultaneously
/// - Within a single session, operations maintain causal ordering
/// - Implementations should use appropriate locking strategies (e.g., per-session locks
///   rather than global locks) to maximize parallelism
///
/// ## Implementation Notes
///
/// For database-backed stores (e.g., SQLite):
/// - Use connection pooling or `Arc<Mutex<Connection>>` with `spawn_blocking`
/// - All queries MUST be scoped by session_id to prevent cross-session data leakage
/// - Use transactions for multi-step operations within a session
/// - Ensure foreign key constraints maintain referential integrity
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Creates a new session, optionally with a name and workspace path.
    async fn create_session(
        &self,
        name: Option<String>,
        cwd: Option<std::path::PathBuf>,
        parent_session_id: Option<String>,
        fork_origin: Option<ForkOrigin>,
    ) -> SessionResult<Session>;

    /// Retrieves session metadata by its unique ID.
    async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>>;

    /// Lists all available sessions.
    async fn list_sessions(&self) -> SessionResult<Vec<Session>>;

    /// Deletes a session and all its associated data.
    async fn delete_session(&self, session_id: &str) -> SessionResult<()>;

    /// Retrieves the rich agent history (including snapshots, reasoning, etc.).
    async fn get_history(&self, session_id: &str) -> SessionResult<Vec<AgentMessage>>;

    /// Appends a rich agent message to the session.
    async fn add_message(&self, session_id: &str, message: AgentMessage) -> SessionResult<()>;

    /// Forks a session from a specific point in history, creating a deep copy of the messages.
    async fn fork_session(
        &self,
        source_session_id: &str,
        target_message_id: &str,
        fork_origin: ForkOrigin,
    ) -> SessionResult<String>;

    /// Create or retrieve an LLM configuration
    async fn create_or_get_llm_config(&self, input: &LLMParams) -> SessionResult<LLMConfig>;

    /// Retrieve an LLM configuration by internal id
    async fn get_llm_config(&self, id: i64) -> SessionResult<Option<LLMConfig>>;

    /// Retrieve the LLM configuration for a session
    async fn get_session_llm_config(&self, session_id: &str) -> SessionResult<Option<LLMConfig>>;

    /// Set the LLM configuration id for a session
    async fn set_session_llm_config(&self, session_id: &str, config_id: i64) -> SessionResult<()>;

    /// Set the provider node for a session (None = local, Some = remote mesh node).
    async fn set_session_provider_node(
        &self,
        session_id: &str,
        provider_node: Option<&str>,
    ) -> SessionResult<()>;

    /// Get the provider node for a session.
    async fn get_session_provider_node(&self, session_id: &str) -> SessionResult<Option<String>>;

    /// Persist the execution configuration snapshot for a session.
    async fn set_session_execution_config(
        &self,
        session_id: &str,
        config: &SessionExecutionConfig,
    ) -> SessionResult<()>;

    /// Retrieve the execution configuration snapshot for a session.
    async fn get_session_execution_config(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<SessionExecutionConfig>>;

    // Phase 3 additions: Repository methods for domain entities

    /// Set the current intent snapshot for a session
    async fn set_current_intent_snapshot(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
    ) -> SessionResult<()>;

    /// Set the active task for a session
    async fn set_active_task(&self, session_id: &str, task_id: Option<&str>) -> SessionResult<()>;

    /// Get fork information for a session
    async fn get_session_fork_info(&self, session_id: &str) -> SessionResult<Option<ForkInfo>>;

    /// List child sessions (forks) of a parent session
    async fn list_child_sessions(&self, parent_id: &str) -> SessionResult<Vec<String>>;

    // Task repository methods
    async fn create_task(&self, task: Task) -> SessionResult<Task>;
    async fn get_task(&self, task_id: &str) -> SessionResult<Option<Task>>;
    async fn list_tasks(&self, session_id: &str) -> SessionResult<Vec<Task>>;
    async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> SessionResult<()>;
    async fn update_task(&self, task: Task) -> SessionResult<()>;
    async fn delete_task(&self, task_id: &str) -> SessionResult<()>;

    // Intent repository methods
    async fn create_intent_snapshot(&self, snapshot: IntentSnapshot) -> SessionResult<()>;
    async fn get_intent_snapshot(&self, snapshot_id: &str)
    -> SessionResult<Option<IntentSnapshot>>;
    async fn list_intent_snapshots(&self, session_id: &str) -> SessionResult<Vec<IntentSnapshot>>;
    async fn get_current_intent_snapshot(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>>;

    // Decision repository methods
    async fn record_decision(&self, decision: Decision) -> SessionResult<()>;
    async fn record_alternative(&self, alternative: Alternative) -> SessionResult<()>;
    async fn get_decision(&self, decision_id: &str) -> SessionResult<Option<Decision>>;
    async fn list_decisions(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Decision>>;
    async fn list_alternatives(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Alternative>>;
    async fn update_decision_status(
        &self,
        decision_id: &str,
        status: DecisionStatus,
    ) -> SessionResult<()>;
    async fn update_alternative_status(
        &self,
        alternative_id: &str,
        status: AlternativeStatus,
    ) -> SessionResult<()>;

    // Progress repository methods
    async fn append_progress_entry(&self, entry: ProgressEntry) -> SessionResult<()>;
    async fn get_progress_entry(&self, entry_id: &str) -> SessionResult<Option<ProgressEntry>>;
    async fn list_progress_entries(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<ProgressEntry>>;
    async fn list_progress_by_kind(
        &self,
        session_id: &str,
        kind: ProgressKind,
    ) -> SessionResult<Vec<ProgressEntry>>;

    // Artifact repository methods
    async fn record_artifact(&self, artifact: Artifact) -> SessionResult<()>;
    async fn get_artifact(&self, artifact_id: &str) -> SessionResult<Option<Artifact>>;
    async fn list_artifacts(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Artifact>>;
    async fn list_artifacts_by_kind(
        &self,
        session_id: &str,
        kind: &str,
    ) -> SessionResult<Vec<Artifact>>;

    // Delegation repository methods
    async fn create_delegation(&self, delegation: Delegation) -> SessionResult<Delegation>;
    async fn get_delegation(&self, delegation_id: &str) -> SessionResult<Option<Delegation>>;
    async fn list_delegations(&self, session_id: &str) -> SessionResult<Vec<Delegation>>;
    async fn update_delegation_status(
        &self,
        delegation_id: &str,
        status: DelegationStatus,
    ) -> SessionResult<()>;
    async fn update_delegation(&self, delegation: Delegation) -> SessionResult<()>;

    // Undo/Redo methods

    /// Get the latest revert state frame for a session (stack top)
    async fn peek_revert_state(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<crate::session::domain::RevertState>>;

    /// Push a revert state frame onto the session stack
    async fn push_revert_state(
        &self,
        session_id: &str,
        state: crate::session::domain::RevertState,
    ) -> SessionResult<()>;

    /// Pop (remove and return) the latest revert state frame for a session
    async fn pop_revert_state(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<crate::session::domain::RevertState>>;

    /// List all revert state frames for a session ordered from oldest to newest.
    async fn list_revert_states(
        &self,
        session_id: &str,
    ) -> SessionResult<Vec<crate::session::domain::RevertState>>;

    /// Clear all revert state frames for a session
    async fn clear_revert_states(&self, session_id: &str) -> SessionResult<()>;

    /// Delete all messages after a given message ID in a session
    /// Returns the number of deleted messages
    async fn delete_messages_after(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> SessionResult<usize>;

    // Compaction methods

    /// Mark tool results as compacted (pruned) by their call IDs.
    ///
    /// This updates the `compacted_at` timestamp for matching ToolResult parts,
    /// causing them to return placeholder text instead of full content when
    /// converted to chat messages.
    ///
    /// # Arguments
    /// * `session_id` - The session containing the messages
    /// * `call_ids` - List of tool call IDs to mark as compacted
    async fn mark_tool_results_compacted(
        &self,
        session_id: &str,
        call_ids: &[String],
    ) -> SessionResult<usize>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_llm_config_values_filters_meta_fields() {
        let params = LLMParams::new()
            .provider("ollama")
            .model("test-model")
            .name("my-config")
            .api_key("secret-key-123")
            .system("test system")
            .temperature(0.7)
            .parameter("num_ctx", 32768);

        let (provider, model, params_json) = extract_llm_config_values(&params).unwrap();

        // Verify provider and model are extracted
        assert_eq!(provider, "ollama");
        assert_eq!(model, "test-model");

        // Verify params JSON excludes provider, model, name, and api_key
        let params_obj = params_json.unwrap();
        let obj = params_obj.as_object().unwrap();

        assert!(
            !obj.contains_key("provider"),
            "provider should be filtered out"
        );
        assert!(!obj.contains_key("model"), "model should be filtered out");
        assert!(!obj.contains_key("name"), "name should be filtered out");
        assert!(
            !obj.contains_key("api_key"),
            "api_key should be filtered out (security!)"
        );

        // Verify other params are included
        assert_eq!(
            obj.get("system").and_then(|v| v.as_array()),
            Some(&vec![Value::String("test system".to_string())])
        );
        // Check temperature with tolerance for f32 -> f64 conversion
        let temp = obj.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!(
            (temp - 0.7).abs() < 0.001,
            "temperature should be approximately 0.7"
        );
        assert_eq!(obj.get("num_ctx").and_then(|v| v.as_i64()), Some(32768));
    }

    #[test]
    fn test_extract_llm_config_values_empty_params() {
        let params = LLMParams::new().provider("openai").model("gpt-4");

        let (provider, model, params_json) = extract_llm_config_values(&params).unwrap();

        assert_eq!(provider, "openai");
        assert_eq!(model, "gpt-4");
        assert!(
            params_json.is_none(),
            "params should be None when only meta fields are present"
        );
    }
}
