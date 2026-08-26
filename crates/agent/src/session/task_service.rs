use std::sync::Arc;

use crate::session::domain::{Task, TaskKind, TaskStatus};
use crate::session::error::{SessionError, SessionResult};
use crate::session::store::{SessionStore, TaskPatch};

#[derive(Clone)]
pub struct TaskService {
    store: Arc<dyn SessionStore>,
    session_id: String,
    tool_call_id: String,
}

impl TaskService {
    pub fn new(store: Arc<dyn SessionStore>, session_id: String, tool_call_id: String) -> Self {
        Self {
            store,
            session_id,
            tool_call_id,
        }
    }

    pub async fn create(
        &self,
        kind: TaskKind,
        expected_deliverable: String,
        acceptance_criteria: Option<String>,
    ) -> SessionResult<Task> {
        self.store
            .create_and_bind_current_task(
                &self.session_id,
                kind,
                expected_deliverable,
                acceptance_criteria,
                &self.tool_call_id,
            )
            .await
    }

    pub async fn read(&self, task_id: Option<&str>) -> SessionResult<Option<Task>> {
        match task_id {
            Some(task_id) => {
                self.store
                    .get_task_for_session(&self.session_id, task_id)
                    .await
            }
            None => self.store.get_current_task(&self.session_id).await,
        }
    }

    pub async fn update(
        &self,
        task_id: &str,
        expected_revision: u64,
        patch: TaskPatch,
        reason: &str,
    ) -> SessionResult<Task> {
        if let Some(status) = patch.status
            && !matches!(
                status,
                TaskStatus::Active | TaskStatus::Paused | TaskStatus::Cancelled
            )
        {
            return Err(SessionError::InvalidOperation(
                "update_task cannot complete a task; use complete_task".to_string(),
            ));
        }
        self.store
            .patch_task_for_session(&self.session_id, task_id, expected_revision, patch, reason)
            .await
    }

    pub async fn complete(
        &self,
        task_id: &str,
        expected_revision: u64,
        completion_evidence: &str,
    ) -> SessionResult<Task> {
        self.store
            .complete_task_for_session(
                &self.session_id,
                task_id,
                expected_revision,
                completion_evidence,
            )
            .await
    }
}
