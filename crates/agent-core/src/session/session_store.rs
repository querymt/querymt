//! The `SessionStore` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::store::*;
use crate::model::AgentMessage;
use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, IntentSnapshot, ProgressEntry, ProgressKind, Task,
    TaskKind, TaskStatus,
};
use crate::session::error::{SessionError, SessionResult};
use crate::session::lifecycle::{SessionLifecycle, SessionLifecyclePatch};
use crate::session_control::SessionControlState;
use async_trait::async_trait;
use querymt::LLMParams;
use std::collections::HashMap;

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

    /// Resolve durable local/remote ownership for a set of session IDs.
    /// Custom stores retain point-lookup semantics unless they provide a batch implementation.
    async fn resolve_session_locations(
        &self,
        session_ids: &[String],
    ) -> SessionResult<HashMap<String, crate::session::location::SessionLocation>> {
        let mut locations = HashMap::with_capacity(session_ids.len());
        for session_id in session_ids {
            let bookmark = self.get_remote_session_bookmark(session_id).await?;
            let local_exists = self.get_session(session_id).await?.is_some();
            locations.insert(
                session_id.clone(),
                crate::session::location::classify_session_location(local_exists, bookmark),
            );
        }
        Ok(locations)
    }

    /// Whether this store provides authoritative persisted session lifecycle state.
    fn supports_session_lifecycle(&self) -> bool {
        false
    }

    /// Read lifecycle state for one existing session.
    async fn get_session_lifecycle(&self, _session_id: &str) -> SessionResult<SessionLifecycle> {
        Err(SessionError::InvalidOperation(
            "Session lifecycle is unsupported by this store".into(),
        ))
    }

    /// Atomically patch lifecycle state for one existing session.
    async fn patch_session_lifecycle(
        &self,
        _session_id: &str,
        _patch: &SessionLifecyclePatch,
    ) -> SessionResult<SessionLifecycle> {
        Err(SessionError::InvalidOperation(
            "Session lifecycle is unsupported by this store".into(),
        ))
    }

    /// Read lifecycle state for a batch of existing sessions.
    async fn get_session_lifecycles(
        &self,
        _session_ids: &[String],
    ) -> SessionResult<HashMap<String, SessionLifecycle>> {
        Err(SessionError::InvalidOperation(
            "Session lifecycle is unsupported by this store".into(),
        ))
    }

    /// Whether this store provides authoritative persisted session pin state.
    fn supports_session_pins(&self) -> bool {
        false
    }

    /// Atomically pin or unpin a locally stored session, returning whether its state changed.
    async fn set_session_pinned(
        &self,
        _session_id: &str,
        _pinned: bool,
    ) -> SessionResult<(SessionPin, bool)> {
        Err(SessionError::InvalidOperation(
            "Session pins are unsupported by this store".into(),
        ))
    }

    /// Batch-read pin state for existing local sessions.
    async fn get_session_pins(
        &self,
        _session_ids: &[String],
    ) -> SessionResult<HashMap<String, SessionPin>> {
        Err(SessionError::InvalidOperation(
            "Session pins are unsupported by this store".into(),
        ))
    }

    /// Persist a session title and return the stored RFC 3339 last-activity
    /// timestamp. `None` explicitly clears the title. Default stores reject
    /// the operation; callers emit metadata updates only after it succeeds.
    async fn set_session_name(
        &self,
        session_id: &str,
        name: Option<String>,
    ) -> SessionResult<String> {
        let _ = (session_id, name);
        Err(crate::session::error::SessionError::InvalidOperation(
            "set_session_name is not supported by this store".to_string(),
        ))
    }

    /// Overwrite a session's durable working directory. Used when a session's
    /// execution workspace changes durably — e.g. an isolated fork binding its
    /// own checkout so the session row, workspace association, listing, load,
    /// and resume all agree. Default stores reject the operation.
    async fn set_session_cwd(
        &self,
        session_id: &str,
        cwd: Option<std::path::PathBuf>,
    ) -> SessionResult<()> {
        let _ = (session_id, cwd);
        Err(crate::session::error::SessionError::InvalidOperation(
            "set_session_cwd is not supported by this store".to_string(),
        ))
    }

    /// None means this storage backend does not support durable assignments.
    /// Supporting backends return an empty snapshot for an existing, unconfigured session.
    async fn get_delegate_assignments(
        &self,
        _session_id: &str,
    ) -> SessionResult<Option<DelegateAssignments>> {
        Ok(None)
    }

    /// Atomically change one role's model, preserving other roles.
    /// Legacy clients may omit expected_revision; clients with a snapshot should supply it.
    async fn set_delegate_assignment(
        &self,
        _session_id: &str,
        _agent_id: &str,
        _model: Option<crate::delegation::model_overrides::DelegateModelOverride>,
        _expected_revision: Option<u64>,
    ) -> SessionResult<DelegateAssignmentWrite> {
        Err(SessionError::InvalidOperation(
            "Durable delegate assignments are unsupported".into(),
        ))
    }

    /// Atomically change one role while preserving an omitted reasoning override.
    async fn set_delegate_assignment_with_reasoning(
        &self,
        session_id: &str,
        agent_id: &str,
        model: Option<crate::delegation::model_overrides::DelegateModelOverride>,
        reasoning_effort: Option<
            Option<crate::delegation::model_overrides::DelegateReasoningEffort>,
        >,
        expected_revision: Option<u64>,
    ) -> SessionResult<DelegateAssignmentWrite> {
        if reasoning_effort.is_some() {
            return Err(SessionError::InvalidOperation(
                "Durable delegate reasoning assignments are unsupported".into(),
            ));
        }
        self.set_delegate_assignment(session_id, agent_id, model, expected_revision)
            .await
    }

    /// Lists all available sessions.
    async fn list_sessions(&self) -> SessionResult<Vec<Session>>;

    /// Deletes a session and all its associated data.
    async fn delete_session(&self, session_id: &str) -> SessionResult<()>;

    /// Retrieves the rich agent history (including snapshots, reasoning, etc.).
    async fn get_history(&self, session_id: &str) -> SessionResult<Vec<AgentMessage>>;

    /// Retrieves the effective agent history — post-compaction, with snapshot-only
    /// messages filtered out. This is the canonical "what matters now" view of
    /// session messages for LLM consumption and delegation handoff.
    async fn get_effective_history(&self, session_id: &str) -> SessionResult<Vec<AgentMessage>> {
        let messages = self.get_history(session_id).await?;
        Ok(crate::history::filter_to_effective_history(messages))
    }

    /// Structured user prompt records (prompt and steering messages) of a
    /// session, in message history order. `message_ids` restricts the result to
    /// those message ids (`None` returns every record).
    ///
    /// The default projects the full history; stores should override it with a
    /// read that touches only prompt content.
    async fn get_user_prompts(
        &self,
        session_id: &str,
        message_ids: Option<&[String]>,
    ) -> SessionResult<Vec<crate::session::prompt_records::UserPromptRecord>> {
        let history = self.get_history(session_id).await?;
        Ok(crate::session::prompt_records::filter_prompt_records(
            crate::session::prompt_records::user_prompt_records(&history),
            message_ids,
        ))
    }

    /// Appends a rich agent message to the session.
    async fn add_message(&self, session_id: &str, message: AgentMessage) -> SessionResult<()>;

    /// Forks a session from a specific point in history, creating a deep copy of the messages.
    async fn fork_session(
        &self,
        source_session_id: &str,
        target_message_id: &str,
        fork_origin: ForkOrigin,
    ) -> SessionResult<String>;

    /// Copy history and persist isolation intent in the same transaction.
    /// Stores without this capability must reject isolated forks before copying.
    async fn fork_session_with_isolation_intent(
        &self,
        source_session_id: &str,
        target_message_id: &str,
        fork_origin: ForkOrigin,
        intent: Option<PendingForkIsolation>,
    ) -> SessionResult<String> {
        if intent.is_some() {
            return Err(SessionError::InvalidOperation(
                "atomic fork isolation intent is unsupported by this store".into(),
            ));
        }
        self.fork_session(source_session_id, target_message_id, fork_origin)
            .await
    }

    /// Create a session and persist its non-runnable isolation barrier in the
    /// same transaction. Stores without this capability must reject isolated
    /// creation before the session row exists.
    async fn create_session_with_isolation_intent(
        &self,
        name: Option<String>,
        cwd: Option<std::path::PathBuf>,
        parent_session_id: Option<String>,
        fork_origin: Option<ForkOrigin>,
        intent: Option<PendingForkIsolation>,
    ) -> SessionResult<Session> {
        if intent.is_some() {
            return Err(SessionError::InvalidOperation(
                "atomic session isolation intent is unsupported by this store".into(),
            ));
        }
        self.create_session(name, cwd, parent_session_id, fork_origin)
            .await
    }

    /// Return the pending isolation intent; absent for legacy local sessions.
    async fn get_pending_fork_isolation(
        &self,
        _session_id: &str,
    ) -> SessionResult<Option<PendingForkIsolation>> {
        Ok(None)
    }

    /// Release the startup barrier only after workspace ownership and cwd persist.
    async fn complete_fork_isolation(&self, _session_id: &str) -> SessionResult<()> {
        Err(SessionError::InvalidOperation(
            "fork isolation completion is unsupported by this store".into(),
        ))
    }

    /// Create or retrieve an LLM configuration
    async fn create_or_get_llm_config(&self, input: &LLMParams) -> SessionResult<LLMConfig>;

    /// Retrieve an LLM configuration by internal id
    async fn get_llm_config(&self, id: i64) -> SessionResult<Option<LLMConfig>>;

    /// Retrieve the LLM configuration for a session
    async fn get_session_llm_config(&self, session_id: &str) -> SessionResult<Option<LLMConfig>>;

    /// Set the LLM configuration id for a session.
    ///
    /// Interactive mode/model/reasoning changes must use `commit_session_control` instead.
    async fn set_session_llm_config(&self, session_id: &str, config_id: i64) -> SessionResult<()>;

    /// Load the persisted mode/model/reasoning state for a session.
    async fn get_session_control(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<SessionControlState>>;

    /// Atomically commit control state, mode bindings, effective model, and provider route.
    async fn commit_session_control(
        &self,
        session_id: &str,
        expected_revision: u64,
        state: &SessionControlState,
    ) -> SessionResult<SessionControlState>;

    /// Persist the profile namespace for a root session.
    async fn set_profile_binding(&self, session_id: &str, profile_id: &str) -> SessionResult<()>;

    /// Persist the complete profile runtime route and provider lock atomically.
    async fn set_session_runtime_binding(
        &self,
        session_id: &str,
        profile_id: &str,
        agent_id: Option<&str>,
        profile_fingerprint: Option<&str>,
        provider_lock_digest: Option<&str>,
        provider_locks_json: Option<&str>,
    ) -> SessionResult<()>;

    /// Retrieve the profile namespace for compatibility with profile ownership checks.
    async fn get_profile_binding(&self, session_id: &str) -> SessionResult<Option<String>>;

    /// Retrieve the complete runtime route for a session.
    async fn get_session_runtime_binding(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<SessionRuntimeBinding>>;

    /// Remove the advisory profile binding for a session.
    async fn remove_profile_binding(&self, session_id: &str) -> SessionResult<()>;

    /// Set the provider node for a session (None = local, Some = remote mesh node).
    async fn set_session_provider_node_id(
        &self,
        session_id: &str,
        provider_node_id: Option<&str>,
    ) -> SessionResult<()>;

    /// Get the provider node for a session.
    async fn get_session_provider_node_id(&self, session_id: &str)
    -> SessionResult<Option<String>>;

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

    /// List user-managed custom models for a provider.
    async fn list_custom_models(&self, provider: &str) -> SessionResult<Vec<CustomModel>>;

    /// Get a user-managed custom model by canonical model_id.
    async fn get_custom_model(
        &self,
        provider: &str,
        model_id: &str,
    ) -> SessionResult<Option<CustomModel>>;

    /// Insert or update a user-managed custom model.
    async fn upsert_custom_model(&self, model: &CustomModel) -> SessionResult<()>;

    /// Delete a user-managed custom model.
    async fn delete_custom_model(&self, provider: &str, model_id: &str) -> SessionResult<()>;

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

    /// Resolve a session's public id and workspace cwd by internal row id
    /// (fork records reference the parent session by internal id).
    async fn get_session_ref_by_internal_id(
        &self,
        session_internal_id: i64,
    ) -> SessionResult<Option<(String, Option<std::path::PathBuf>)>>;

    /// List child sessions (forks) of a parent session
    async fn list_child_sessions(&self, parent_id: &str) -> SessionResult<Vec<String>>;

    // Task repository methods
    async fn create_task(&self, task: Task) -> SessionResult<Task>;
    async fn get_task(&self, task_id: &str) -> SessionResult<Option<Task>>;
    async fn get_current_task(&self, session_id: &str) -> SessionResult<Option<Task>> {
        let session = self.get_session(session_id).await?;
        let Some(current_id) = session.and_then(|session| session.active_task_id) else {
            return Ok(None);
        };
        Ok(self
            .list_tasks(session_id)
            .await?
            .into_iter()
            .find(|task| task.id == current_id)
            .filter(|task| matches!(task.status, TaskStatus::Active)))
    }
    async fn get_task_for_session(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> SessionResult<Option<Task>> {
        let session = self.get_session(session_id).await?;
        let Some(session) = session else {
            return Ok(None);
        };
        Ok(self
            .get_task(task_id)
            .await?
            .filter(|task| task.session_id == session.id))
    }
    async fn create_and_bind_current_task(
        &self,
        _session_id: &str,
        _kind: TaskKind,
        _expected_deliverable: String,
        _acceptance_criteria: Option<String>,
        _creation_key: &str,
    ) -> SessionResult<Task> {
        Err(SessionError::InvalidOperation(
            "atomic task creation is unsupported".to_string(),
        ))
    }
    async fn patch_task_for_session(
        &self,
        _session_id: &str,
        _task_id: &str,
        _expected_revision: u64,
        _patch: TaskPatch,
        _reason: &str,
    ) -> SessionResult<Task> {
        Err(SessionError::InvalidOperation(
            "atomic task patch is unsupported".to_string(),
        ))
    }
    async fn complete_task_for_session(
        &self,
        _session_id: &str,
        _task_id: &str,
        _expected_revision: u64,
        _completion_evidence: &str,
    ) -> SessionResult<Task> {
        Err(SessionError::InvalidOperation(
            "atomic task completion is unsupported".to_string(),
        ))
    }
    async fn list_tasks(&self, session_id: &str) -> SessionResult<Vec<Task>>;
    async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> SessionResult<()>;
    async fn update_task(&self, task: Task) -> SessionResult<()>;
    async fn delete_task(&self, task_id: &str) -> SessionResult<()>;

    // Intent repository methods
    async fn create_intent_snapshot(&self, snapshot: IntentSnapshot) -> SessionResult<()>;
    /// Atomically persist an intent snapshot and bind it as current.
    /// Backends without transactional support must reject this operation.
    async fn create_and_set_current_intent_snapshot(
        &self,
        _session_id: &str,
        _snapshot: IntentSnapshot,
    ) -> SessionResult<IntentSnapshot> {
        Err(SessionError::InvalidOperation(
            "atomic intent snapshot activation is not supported by this backend".to_string(),
        ))
    }
    async fn get_intent_snapshot(&self, snapshot_id: &str)
    -> SessionResult<Option<IntentSnapshot>>;
    async fn list_intent_snapshots(&self, session_id: &str) -> SessionResult<Vec<IntentSnapshot>>;
    async fn get_initial_intent_snapshot(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>>;
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
    /// Persist the initiating authority atomically with a deferred delegation.
    async fn create_delegation_with_origin(
        &self,
        delegation: Delegation,
        origin: super::domain_schedule::ScheduleOrigin,
    ) -> SessionResult<Delegation> {
        use super::domain_schedule::ScheduleOrigin;
        if origin != ScheduleOrigin::Local {
            return Err(SessionError::InvalidOperation(
                "delegation origin storage unavailable".into(),
            ));
        }
        self.create_delegation(delegation).await
    }

    async fn delegation_origin(
        &self,
        _delegation_id: &str,
    ) -> SessionResult<super::domain_schedule::ScheduleOrigin> {
        // Backends that don't implement origin storage cannot admit peer work.
        Ok(super::domain_schedule::ScheduleOrigin::Local)
    }

    async fn get_delegation(&self, delegation_id: &str) -> SessionResult<Option<Delegation>>;
    async fn list_delegations(&self, session_id: &str) -> SessionResult<Vec<Delegation>>;
    async fn claim_delegation(
        &self,
        delegation_id: &str,
    ) -> SessionResult<crate::session::domain::DelegationClaim>;
    async fn transition_delegation_status(
        &self,
        delegation_id: &str,
        expected: DelegationStatus,
        next: DelegationStatus,
    ) -> SessionResult<bool>;
    async fn update_delegation(&self, delegation: Delegation) -> SessionResult<()>;
    async fn set_delegation_planning_summary(
        &self,
        delegation_id: &str,
        planning_summary: &str,
    ) -> SessionResult<()>;

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

    /// Clear all revert state frames for a session, including the recovery
    /// frames of failed restores.
    async fn clear_revert_states(&self, session_id: &str) -> SessionResult<()>;

    /// Delete the message with the given ID and all messages after it.
    /// Returns the number of deleted messages.
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

    // ── Remote session bookmarks ─────────────────────────────────────────

    /// Persist (insert or update) a remote session bookmark.
    async fn save_remote_session_bookmark(
        &self,
        bookmark: &RemoteSessionBookmark,
    ) -> SessionResult<()>;

    /// Point lookup for a single remote session bookmark by session ID.
    ///
    /// This is the authoritative durable identity check for remote sessions:
    /// a bookmark here means the ID belongs to a remote session regardless of
    /// whether an attachment is currently installed in the registry.
    async fn get_remote_session_bookmark(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<RemoteSessionBookmark>>;

    /// List all persisted remote session bookmarks.
    async fn list_remote_session_bookmarks(&self) -> SessionResult<Vec<RemoteSessionBookmark>>;

    /// Remove a remote session bookmark by session ID.
    async fn remove_remote_session_bookmark(&self, session_id: &str) -> SessionResult<()>;
}
