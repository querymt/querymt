use crate::model::AgentMessage;
use crate::session::domain::{
    Alternative, AlternativeStatus, Artifact, Decision, DecisionStatus, Delegation,
    DelegationStatus, ForkOrigin, ForkPointType, IntentSnapshot, ProgressEntry, ProgressKind, Task,
    TaskKind, TaskStatus,
};
use crate::session::error::{SessionError, SessionResult};
use crate::session::store::SessionStore;
use serde_json::Value;
use std::sync::Arc;
use time::OffsetDateTime;

/// RuntimeContext provides access to all repositories and runtime state
pub struct RuntimeContext {
    pub store: Arc<dyn SessionStore>,
    pub session_id: String,
    session_internal_id: Option<i64>,
    /// Cached active task (loaded at start of cycle)
    pub active_task: Option<Task>,
    /// Cached current intent snapshot (loaded at start of cycle)
    pub current_intent: Option<IntentSnapshot>,
}

impl RuntimeContext {
    /// Create a new RuntimeContext for a session
    pub async fn new(store: Arc<dyn SessionStore>, session_id: String) -> SessionResult<Self> {
        Ok(Self {
            store,
            session_id,
            session_internal_id: None,
            active_task: None,
            current_intent: None,
        })
    }

    async fn ensure_session_internal_id(&mut self) -> SessionResult<i64> {
        if let Some(id) = self.session_internal_id {
            return Ok(id);
        }
        let session = self
            .store
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?;
        self.session_internal_id = Some(session.id);
        Ok(session.id)
    }

    fn get_internal_task_id(&self) -> Option<i64> {
        self.active_task.as_ref().map(|task| task.id)
    }

    async fn load_task_by_internal_id(&self, task_internal_id: i64) -> SessionResult<Option<Task>> {
        let tasks = self.store.list_tasks(&self.session_id).await?;
        Ok(tasks.into_iter().find(|task| task.id == task_internal_id))
    }

    /// Load active task and current intent snapshot before agent cycle
    pub async fn load_working_context(&mut self) -> SessionResult<()> {
        let session = self
            .store
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?;

        self.session_internal_id = Some(session.id);

        if let Some(task_id) = session.active_task_id {
            self.active_task = self.load_task_by_internal_id(task_id).await?;
        } else {
            self.active_task = None;
        }

        if let Some(snapshot_id) = session.current_intent_snapshot_id {
            self.current_intent = self
                .store
                .get_intent_snapshot(&snapshot_id.to_string())
                .await?;
        } else {
            self.current_intent = self
                .store
                .get_current_intent_snapshot(&self.session_id)
                .await?;
        }

        Ok(())
    }

    /// Update intent snapshot when user intent changes
    pub async fn update_intent_snapshot(
        &mut self,
        summary: String,
        constraints: Option<String>,
        next_step_hint: Option<String>,
    ) -> SessionResult<IntentSnapshot> {
        let session_internal_id = self.ensure_session_internal_id().await?;

        let snapshot = IntentSnapshot {
            id: 0,
            session_id: session_internal_id,
            task_id: self.get_internal_task_id(),
            summary,
            constraints,
            next_step_hint,
            created_at: OffsetDateTime::now_utc(),
        };

        self.store.create_intent_snapshot(snapshot.clone()).await?;
        let latest_snapshot = self
            .store
            .get_current_intent_snapshot(&self.session_id)
            .await?
            .ok_or_else(|| {
                SessionError::IntentSnapshotNotFound("latest snapshot missing".to_string())
            })?;

        let snapshot_id_str = latest_snapshot.id.to_string();
        self.store
            .set_current_intent_snapshot(&self.session_id, Some(&snapshot_id_str))
            .await?;
        self.current_intent = Some(latest_snapshot.clone());

        Ok(latest_snapshot)
    }

    /// Create a new task and set it as active
    pub async fn create_and_set_active_task(
        &mut self,
        kind: TaskKind,
        expected_deliverable: Option<String>,
        acceptance_criteria: Option<String>,
    ) -> SessionResult<Task> {
        let session_internal_id = self.ensure_session_internal_id().await?;
        let public_id = uuid::Uuid::now_v7().to_string();
        let task = Task {
            id: 0,
            public_id: public_id.clone(),
            session_id: session_internal_id,
            kind,
            status: TaskStatus::Active,
            expected_deliverable,
            acceptance_criteria,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };

        let stored_task = self.store.create_task(task).await?;

        self.store
            .set_active_task(&self.session_id, Some(&stored_task.public_id))
            .await?;
        self.active_task = Some(stored_task.clone());

        Ok(stored_task)
    }

    /// Record a decision made during execution
    pub async fn record_decision(
        &self,
        description: String,
        rationale: Option<String>,
        status: DecisionStatus,
    ) -> SessionResult<Decision> {
        let session_internal_id = self
            .store
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?
            .id;

        let decision = Decision {
            id: 0,
            session_id: session_internal_id,
            task_id: self.get_internal_task_id(),
            description,
            rationale,
            status,
            created_at: OffsetDateTime::now_utc(),
        };

        self.store.record_decision(decision.clone()).await?;
        Ok(decision)
    }

    /// Record an alternative approach
    pub async fn record_alternative(
        &self,
        description: String,
        status: AlternativeStatus,
    ) -> SessionResult<Alternative> {
        let session_internal_id = self
            .store
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?
            .id;

        let alternative = Alternative {
            id: 0,
            session_id: session_internal_id,
            task_id: self.get_internal_task_id(),
            description,
            status,
            created_at: OffsetDateTime::now_utc(),
        };

        self.store.record_alternative(alternative.clone()).await?;
        Ok(alternative)
    }

    /// Record a progress entry (e.g., tool call, checkpoint, note)
    pub async fn record_progress(
        &self,
        kind: ProgressKind,
        content: String,
        metadata: Option<Value>,
    ) -> SessionResult<ProgressEntry> {
        let session_internal_id = self
            .store
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?
            .id;

        let entry = ProgressEntry {
            id: 0,
            session_id: session_internal_id,
            task_id: self.get_internal_task_id(),
            kind,
            content,
            metadata: metadata.map(|m| serde_json::to_string(&m).unwrap_or_default()),
            created_at: OffsetDateTime::now_utc(),
        };

        self.store.append_progress_entry(entry.clone()).await?;
        Ok(entry)
    }

    /// Record an artifact produced during execution
    pub async fn record_artifact(
        &self,
        kind: String,
        uri: Option<String>,
        path: Option<String>,
        summary: Option<String>,
    ) -> SessionResult<Artifact> {
        let session_internal_id = self
            .store
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?
            .id;

        let artifact = Artifact {
            id: 0,
            session_id: session_internal_id,
            task_id: self.get_internal_task_id(),
            kind,
            uri,
            path,
            summary,
            created_at: OffsetDateTime::now_utc(),
        };

        self.store.record_artifact(artifact.clone()).await?;
        Ok(artifact)
    }

    /// Record a delegation to another agent
    pub async fn record_delegation(
        &self,
        target_agent_id: String,
        objective: String,
        context: Option<String>,
        constraints: Option<String>,
        expected_output: Option<String>,
    ) -> SessionResult<Delegation> {
        let session_internal_id = self
            .store
            .get_session(&self.session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?
            .id;

        let objective_hash = crate::hash::RapidHash::new(objective.as_bytes());
        let retry_count = self
            .store
            .list_delegations(&self.session_id)
            .await?
            .iter()
            .filter(|d| {
                d.objective_hash == objective_hash
                    && d.target_agent_id == target_agent_id
                    && d.status == DelegationStatus::Failed
            })
            .map(|d| d.retry_count)
            .max()
            .map(|max_retry| max_retry + 1)
            .unwrap_or(0);

        let public_id = uuid::Uuid::now_v7().to_string();
        let delegation = Delegation {
            id: 0,
            public_id: public_id.clone(),
            session_id: session_internal_id,
            task_id: self.get_internal_task_id(),
            target_agent_id,
            objective,
            objective_hash,
            context,
            constraints,
            expected_output,
            status: DelegationStatus::Requested,
            verification_spec: None,
            planning_summary: None,
            retry_count,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
        };

        let stored_delegation = self.store.create_delegation(delegation).await?;
        Ok(stored_delegation)
    }

    /// Update delegation status
    pub async fn update_delegation_status(
        &self,
        delegation_id: &str,
        status: DelegationStatus,
    ) -> SessionResult<()> {
        self.store
            .update_delegation_status(delegation_id, status)
            .await?;
        Ok(())
    }

    /// Update task status
    pub async fn update_task_status(&mut self, status: TaskStatus) -> SessionResult<()> {
        if let Some(task) = &mut self.active_task {
            self.store
                .update_task_status(&task.public_id, status)
                .await?;
            task.status = status;
            task.updated_at = OffsetDateTime::now_utc();
        }
        Ok(())
    }
}

/// Session forking utilities
pub struct SessionForkHelper;

impl SessionForkHelper {
    /// Initialize a forked session by reconstructing state at fork point
    pub async fn initialize_fork(
        store: Arc<dyn SessionStore>,
        session_id: &str,
    ) -> SessionResult<()> {
        let _session = store
            .get_session(session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(session_id.to_string()))?;

        let fork_info = store.get_session_fork_info(session_id).await?;
        let Some(info) = fork_info else {
            return Ok(());
        };

        if matches!(info.fork_origin, Some(ForkOrigin::Delegation)) {
            return Ok(());
        }

        let Some(parent_id) = info.parent_session_id else {
            return Ok(());
        };

        let Some(fork_point_type) = info.fork_point_type else {
            return Ok(());
        };

        let Some(fork_point_ref) = info.fork_point_ref else {
            return Ok(());
        };

        // Reconstruct state at fork point
        match fork_point_type {
            ForkPointType::MessageIndex => {
                Self::reconstruct_at_message_index(
                    store.clone(),
                    &parent_id.to_string(),
                    session_id,
                    &fork_point_ref,
                    &info.fork_instructions,
                )
                .await?;
            }
            ForkPointType::ProgressEntry => {
                Self::reconstruct_at_progress_entry(
                    store.clone(),
                    &parent_id.to_string(),
                    session_id,
                    &fork_point_ref,
                    &info.fork_instructions,
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Reconstruct session state at a message index
    async fn reconstruct_at_message_index(
        store: Arc<dyn SessionStore>,
        parent_id: &str,
        child_id: &str,
        message_index: &str,
        fork_instructions: &Option<String>,
    ) -> SessionResult<()> {
        let index: usize = message_index.parse().map_err(|e| {
            SessionError::InvalidMessageIndex(format!("Invalid message index: {}", e))
        })?;

        let parent_messages = store.get_history(parent_id).await?;
        let truncated_messages: Vec<AgentMessage> =
            parent_messages.into_iter().take(index + 1).collect();

        for message in truncated_messages {
            let mut child_message = message;
            child_message.id = uuid::Uuid::now_v7().to_string();
            child_message.session_id = child_id.to_string();
            store.add_message(child_id, child_message).await?;
        }

        Self::create_fork_intent_snapshot(store, parent_id, child_id, fork_instructions).await?;

        Ok(())
    }

    /// Reconstruct session state at a progress entry
    async fn reconstruct_at_progress_entry(
        store: Arc<dyn SessionStore>,
        parent_id: &str,
        child_id: &str,
        progress_entry_id: &str,
        fork_instructions: &Option<String>,
    ) -> SessionResult<()> {
        let progress_entry = store
            .get_progress_entry(progress_entry_id)
            .await?
            .ok_or_else(|| SessionError::ProgressEntryNotFound(progress_entry_id.to_string()))?;

        let cutoff_time = progress_entry.created_at;

        let parent_messages = store.get_history(parent_id).await?;
        for message in parent_messages {
            if message.created_at <= cutoff_time.unix_timestamp() {
                let mut child_message = message;
                child_message.id = uuid::Uuid::now_v7().to_string();
                child_message.session_id = child_id.to_string();
                store.add_message(child_id, child_message).await?;
            }
        }

        let progress_entries = store.list_progress_entries(parent_id, None).await?;
        for entry in progress_entries {
            if entry.created_at <= cutoff_time {
                let mut child_entry = entry;
                child_entry.id = 0;
                child_entry.session_id = match store.get_session(child_id).await? {
                    Some(session) => session.id,
                    None => {
                        return Err(SessionError::SessionNotFound(child_id.to_string()));
                    }
                };
                store.append_progress_entry(child_entry).await?;
            }
        }

        Self::create_fork_intent_snapshot(store, parent_id, child_id, fork_instructions).await?;

        Ok(())
    }

    /// Create combined IntentSnapshot for forked session
    async fn create_fork_intent_snapshot(
        store: Arc<dyn SessionStore>,
        parent_id: &str,
        child_id: &str,
        fork_instructions: &Option<String>,
    ) -> SessionResult<()> {
        let parent_intent = store.get_current_intent_snapshot(parent_id).await?;

        let summary = if let Some(parent_intent) = parent_intent {
            if let Some(instructions) = fork_instructions {
                format!(
                    "{}\n\nFORK INSTRUCTIONS:\n{}",
                    parent_intent.summary, instructions
                )
            } else {
                format!("{}\n\n(Forked session)", parent_intent.summary)
            }
        } else if let Some(instructions) = fork_instructions {
            format!("Forked session with new instructions:\n{}", instructions)
        } else {
            "Forked session".to_string()
        };

        let child_session = store
            .get_session(child_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(child_id.to_string()))?;

        let snapshot = IntentSnapshot {
            id: 0,
            session_id: child_session.id,
            task_id: None,
            summary,
            constraints: Some(
                "This is a forked session. Focus on the fork instructions.".to_string(),
            ),
            next_step_hint: None,
            created_at: OffsetDateTime::now_utc(),
        };

        store.create_intent_snapshot(snapshot).await?;
        let latest_snapshot = store
            .get_current_intent_snapshot(child_id)
            .await?
            .ok_or_else(|| SessionError::IntentSnapshotNotFound(child_id.to_string()))?;
        let snapshot_id = latest_snapshot.id.to_string();
        store
            .set_current_intent_snapshot(child_id, Some(&snapshot_id))
            .await?;

        Ok(())
    }
}
