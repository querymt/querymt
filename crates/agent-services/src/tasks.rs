use crate::session::domain::TaskStatus;
use crate::session::error::{SessionError, SessionResult};
use crate::session::store::SessionStore;
use std::sync::Arc;
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum TaskWaitOutcome {
    Completed {
        task_id: String,
        deliverable: Option<String>,
    },
    Cancelled {
        task_id: String,
    },
    Cleared {
        last_task_id: String,
    },
    NoTaskCreated,
    TimedOut,
}

pub struct TaskWatcher {
    store: Arc<dyn SessionStore>,
    session_id: String,
    poll_interval: Duration,
    idle_timeout: Duration,
    total_timeout: Duration,
}

impl TaskWatcher {
    pub fn new(store: Arc<dyn SessionStore>, session_id: impl Into<String>) -> Self {
        Self {
            store,
            session_id: session_id.into(),
            poll_interval: Duration::from_millis(100),
            idle_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(300),
        }
    }

    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    pub fn with_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    pub fn with_total_timeout(mut self, total_timeout: Duration) -> Self {
        self.total_timeout = total_timeout;
        self
    }

    pub async fn wait(&self) -> SessionResult<TaskWaitOutcome> {
        let start_time = Instant::now();
        let mut no_task_start: Option<Instant> = None;
        let mut last_task_id: Option<String> = None;

        loop {
            if start_time.elapsed() > self.total_timeout {
                return Ok(TaskWaitOutcome::TimedOut);
            }

            let session = self
                .store
                .get_session(&self.session_id)
                .await?
                .ok_or_else(|| SessionError::SessionNotFound(self.session_id.clone()))?;

            if let Some(task_internal_id) = session.active_task_id {
                no_task_start = None;

                let tasks = self.store.list_tasks(&self.session_id).await?;
                let task = tasks
                    .into_iter()
                    .find(|task| task.id == task_internal_id)
                    .ok_or_else(|| SessionError::TaskNotFound(task_internal_id.to_string()))?;
                let task_public_id = task.public_id.clone();

                last_task_id = Some(task_public_id.clone());

                match task.status {
                    TaskStatus::Done => {
                        return Ok(TaskWaitOutcome::Completed {
                            task_id: task_public_id,
                            deliverable: task.expected_deliverable,
                        });
                    }
                    TaskStatus::Cancelled => {
                        return Ok(TaskWaitOutcome::Cancelled {
                            task_id: task_public_id,
                        });
                    }
                    TaskStatus::Active | TaskStatus::Paused => {}
                }
            } else if let Some(task_id) = last_task_id.clone() {
                let task = self
                    .store
                    .get_task(&task_id)
                    .await?
                    .ok_or_else(|| SessionError::TaskNotFound(task_id.clone()))?;

                match task.status {
                    TaskStatus::Done => {
                        return Ok(TaskWaitOutcome::Completed {
                            task_id,
                            deliverable: task.expected_deliverable,
                        });
                    }
                    TaskStatus::Cancelled => {
                        return Ok(TaskWaitOutcome::Cancelled { task_id });
                    }
                    TaskStatus::Active | TaskStatus::Paused => {
                        return Ok(TaskWaitOutcome::Cleared {
                            last_task_id: task_id,
                        });
                    }
                }
            } else {
                let idle_start = no_task_start.get_or_insert_with(Instant::now);
                if idle_start.elapsed() > self.idle_timeout {
                    return Ok(TaskWaitOutcome::NoTaskCreated);
                }
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TaskWaitOutcome construction ───────────────────────────────────────

    #[test]
    fn task_wait_outcome_completed_construction() {
        let outcome = TaskWaitOutcome::Completed {
            task_id: "task-1".to_string(),
            deliverable: Some("test output".to_string()),
        };
        match outcome {
            TaskWaitOutcome::Completed {
                task_id,
                deliverable,
            } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(deliverable, Some("test output".to_string()));
            }
            _ => panic!("expected Completed variant"),
        }
    }

    #[test]
    fn task_wait_outcome_cancelled_construction() {
        let outcome = TaskWaitOutcome::Cancelled {
            task_id: "task-2".to_string(),
        };
        match outcome {
            TaskWaitOutcome::Cancelled { task_id } => {
                assert_eq!(task_id, "task-2");
            }
            _ => panic!("expected Cancelled variant"),
        }
    }

    #[test]
    fn task_wait_outcome_cleared_construction() {
        let outcome = TaskWaitOutcome::Cleared {
            last_task_id: "task-3".to_string(),
        };
        match outcome {
            TaskWaitOutcome::Cleared { last_task_id } => {
                assert_eq!(last_task_id, "task-3");
            }
            _ => panic!("expected Cleared variant"),
        }
    }

    #[test]
    fn task_wait_outcome_no_task_created() {
        let outcome = TaskWaitOutcome::NoTaskCreated;
        assert!(matches!(outcome, TaskWaitOutcome::NoTaskCreated));
    }

    #[test]
    fn task_wait_outcome_timed_out() {
        let outcome = TaskWaitOutcome::TimedOut;
        assert!(matches!(outcome, TaskWaitOutcome::TimedOut));
    }
}
