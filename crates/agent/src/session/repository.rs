//! Repository trait interfaces for domain entities.
//! These define the data access layer for the refactored agent system.

use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkInfo, ForkOrigin, ForkPointType, IntentSnapshot, ProgressEntry,
    ProgressKind, Task, TaskStatus,
};
use crate::session::error::SessionResult;
use crate::session::store::Session;
use async_trait::async_trait;

/// Repository for session management
#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// Create a new session
    async fn create_session(
        &self,
        name: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> SessionResult<Session>;

    /// Get session by ID
    async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>>;

    /// List all sessions
    async fn list_sessions(&self) -> SessionResult<Vec<Session>>;

    /// Delete a session and all associated data
    async fn delete_session(&self, session_id: &str) -> SessionResult<()>;

    /// Set the current intent snapshot for a session
    async fn set_current_intent_snapshot(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
    ) -> SessionResult<()>;

    /// Set the active task for a session
    async fn set_active_task(&self, session_id: &str, task_id: Option<&str>) -> SessionResult<()>;

    /// Fork a session from a specific point
    async fn fork_session(
        &self,
        parent_id: &str,
        fork_point_type: ForkPointType,
        fork_point_ref: &str,
        fork_origin: ForkOrigin,
        additional_instructions: Option<String>,
    ) -> SessionResult<String>;

    /// Get fork information for a session
    async fn get_session_fork_info(&self, session_id: &str) -> SessionResult<Option<ForkInfo>>;

    /// List child sessions (forks) of a parent session
    async fn list_child_sessions(&self, parent_id: &str) -> SessionResult<Vec<String>>;
}

/// Repository for task management
#[async_trait]
pub trait TaskRepository: Send + Sync {
    /// Create a new task
    async fn create_task(&self, task: Task) -> SessionResult<Task>;

    /// Get task by ID
    async fn get_task(&self, task_id: &str) -> SessionResult<Option<Task>>;

    /// List tasks for a session
    async fn list_tasks(&self, session_id: &str) -> SessionResult<Vec<Task>>;

    /// Update task status
    async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> SessionResult<()>;

    /// Update task fields
    async fn update_task(&self, task: Task) -> SessionResult<()>;

    /// Delete a task
    async fn delete_task(&self, task_id: &str) -> SessionResult<()>;
}

/// Repository for intent snapshots
#[async_trait]
pub trait IntentRepository: Send + Sync {
    /// Create a new intent snapshot
    async fn create_intent_snapshot(&self, snapshot: IntentSnapshot) -> SessionResult<()>;

    /// Get intent snapshot by ID
    async fn get_intent_snapshot(&self, snapshot_id: &str)
    -> SessionResult<Option<IntentSnapshot>>;

    /// List intent snapshots for a session
    async fn list_intent_snapshots(&self, session_id: &str) -> SessionResult<Vec<IntentSnapshot>>;

    /// Get the most recent intent snapshot for a session
    async fn get_current_intent_snapshot(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>>;
}

/// Repository for decisions and alternatives
#[async_trait]
pub trait DecisionRepository: Send + Sync {
    /// Record a decision
    async fn record_decision(&self, decision: Decision) -> SessionResult<()>;

    /// Record an alternative
    async fn record_alternative(&self, alternative: Alternative) -> SessionResult<()>;

    /// Get decision by ID
    async fn get_decision(&self, decision_id: &str) -> SessionResult<Option<Decision>>;

    /// List decisions for a session or task
    async fn list_decisions(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Decision>>;

    /// List alternatives for a session or task
    async fn list_alternatives(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Alternative>>;

    /// Update decision status
    async fn update_decision_status(
        &self,
        decision_id: &str,
        status: DecisionStatus,
    ) -> SessionResult<()>;

    /// Update alternative status
    async fn update_alternative_status(
        &self,
        alternative_id: &str,
        status: AlternativeStatus,
    ) -> SessionResult<()>;
}

/// Repository for progress tracking
#[async_trait]
pub trait ProgressRepository: Send + Sync {
    /// Append a progress entry
    async fn append_progress_entry(&self, entry: ProgressEntry) -> SessionResult<()>;

    /// Get progress entry by ID
    async fn get_progress_entry(&self, entry_id: &str) -> SessionResult<Option<ProgressEntry>>;

    /// List progress entries for a session
    async fn list_progress_entries(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<ProgressEntry>>;

    /// List progress entries by kind
    async fn list_progress_by_kind(
        &self,
        session_id: &str,
        kind: ProgressKind,
    ) -> SessionResult<Vec<ProgressEntry>>;
}

/// Repository for artifacts
#[async_trait]
pub trait ArtifactRepository: Send + Sync {
    /// Record an artifact
    async fn record_artifact(&self, artifact: Artifact) -> SessionResult<()>;

    /// Get artifact by ID
    async fn get_artifact(&self, artifact_id: &str) -> SessionResult<Option<Artifact>>;

    /// List artifacts for a session
    async fn list_artifacts(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Artifact>>;

    /// List artifacts by kind
    async fn list_artifacts_by_kind(
        &self,
        session_id: &str,
        kind: &str,
    ) -> SessionResult<Vec<Artifact>>;
}

/// Repository for delegations
#[async_trait]
pub trait DelegationRepository: Send + Sync {
    /// Create a delegation
    async fn create_delegation(&self, delegation: Delegation) -> SessionResult<Delegation>;

    /// Get delegation by ID
    async fn get_delegation(&self, delegation_id: &str) -> SessionResult<Option<Delegation>>;

    /// List delegations for a session
    async fn list_delegations(&self, session_id: &str) -> SessionResult<Vec<Delegation>>;

    /// Update delegation status
    async fn update_delegation_status(
        &self,
        delegation_id: &str,
        status: DelegationStatus,
    ) -> SessionResult<()>;

    /// Update delegation
    async fn update_delegation(&self, delegation: Delegation) -> SessionResult<()>;
}
