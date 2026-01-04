//! SQLite implementation of TaskRepository

use crate::session::domain::{Task, TaskKind, TaskStatus};
use crate::session::error::{SessionError, SessionResult};
use crate::session::repository::TaskRepository;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// SQLite implementation of TaskRepository
#[derive(Clone)]
pub struct SqliteTaskRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteTaskRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    async fn run_blocking<F, R>(&self, f: F) -> SessionResult<R>
    where
        F: FnOnce(&mut Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        let conn_arc = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn_arc.lock().unwrap();
            f(&mut conn)
        })
        .await
        .map_err(|e| SessionError::Other(format!("Task execution failed: {}", e)))?
        .map_err(SessionError::from)
    }
}

impl fmt::Display for TaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskKind::Finite => write!(f, "finite"),
            TaskKind::Recurring => write!(f, "recurring"),
            TaskKind::Evolving => write!(f, "evolving"),
        }
    }
}

impl FromStr for TaskKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "finite" => Ok(TaskKind::Finite),
            "recurring" => Ok(TaskKind::Recurring),
            "evolving" => Ok(TaskKind::Evolving),
            _ => Err(format!("Unknown task kind: {}", s)),
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskStatus::Active => write!(f, "active"),
            TaskStatus::Paused => write!(f, "paused"),
            TaskStatus::Done => write!(f, "done"),
            TaskStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl FromStr for TaskStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(TaskStatus::Active),
            "paused" => Ok(TaskStatus::Paused),
            "done" => Ok(TaskStatus::Done),
            "cancelled" => Ok(TaskStatus::Cancelled),
            _ => Err(format!("Unknown task status: {}", s)),
        }
    }
}

#[async_trait]
impl TaskRepository for SqliteTaskRepository {
    async fn create_task(&self, mut task: Task) -> SessionResult<Task> {
        let task = self.run_blocking(move |conn| {
            // Generate UUID v7 for public_id if not set
            if task.public_id.is_empty() {
                task.public_id = uuid::Uuid::now_v7().to_string();
            }

            conn.execute(
                "INSERT INTO tasks (public_id, session_id, kind, status, expected_deliverable, acceptance_criteria, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    task.public_id,
                    task.session_id,
                    task.kind.to_string(),
                    task.status.to_string(),
                    task.expected_deliverable,
                    task.acceptance_criteria,
                    task.created_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
                    task.updated_at.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
                ],
            )?;
            task.id = conn.last_insert_rowid();
            Ok::<Task, rusqlite::Error>(task)
        })
        .await?;

        Ok(task)
    }

    async fn get_task(&self, task_id: &str) -> SessionResult<Option<Task>> {
        let task_id_str = task_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, public_id, session_id, kind, status, expected_deliverable, acceptance_criteria, created_at, updated_at FROM tasks WHERE public_id = ?",
                params![task_id_str],
                |row| {
                    let kind_str: String = row.get(3)?;
                    let status_str: String = row.get(4)?;
                    let created_at_str: String = row.get(7)?;
                    let updated_at_str: String = row.get(8)?;

                    Ok(Task {
                        id: row.get(0)?,
                        public_id: row.get(1)?,
                        session_id: row.get(2)?,
                        kind: TaskKind::from_str(&kind_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        status: TaskStatus::from_str(&status_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        expected_deliverable: row.get(5)?,
                        acceptance_criteria: row.get(6)?,
                        created_at: OffsetDateTime::parse(&created_at_str, &time::format_description::well_known::Rfc3339).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        updated_at: OffsetDateTime::parse(&updated_at_str, &time::format_description::well_known::Rfc3339).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    })
                },
            )
            .optional()
        })
        .await
    }

    async fn list_tasks(&self, session_id: &str) -> SessionResult<Vec<Task>> {
        let session_id_str = session_id.to_string();
        self.run_blocking(move |conn| {
            // First resolve public_id to internal id
            let internal_id: Option<i64> = conn.query_row(
                "SELECT id FROM sessions WHERE public_id = ?",
                params![session_id_str],
                |row| row.get(0)
            ).optional()?;

            let internal_id = internal_id.ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;

            let mut stmt = conn.prepare(
                "SELECT id, public_id, session_id, kind, status, expected_deliverable, acceptance_criteria, created_at, updated_at FROM tasks WHERE session_id = ? ORDER BY created_at ASC",
            )?;
            let tasks_iter = stmt.query_map(params![internal_id], |row| {
                let kind_str: String = row.get(3)?;
                let status_str: String = row.get(4)?;
                let created_at_str: String = row.get(7)?;
                let updated_at_str: String = row.get(8)?;

                Ok(Task {
                    id: row.get(0)?,
                    public_id: row.get(1)?,
                    session_id: row.get(2)?,
                    kind: TaskKind::from_str(&kind_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    status: TaskStatus::from_str(&status_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    expected_deliverable: row.get(5)?,
                    acceptance_criteria: row.get(6)?,
                    created_at: OffsetDateTime::parse(&created_at_str, &time::format_description::well_known::Rfc3339).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    updated_at: OffsetDateTime::parse(&updated_at_str, &time::format_description::well_known::Rfc3339).map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            })?;

            tasks_iter.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> SessionResult<()> {
        let task_id_str = task_id.to_string();
        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE tasks SET status = ?, updated_at = ? WHERE public_id = ?",
                params![
                    status.to_string(),
                    OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                    task_id_str
                ],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => SessionError::TaskNotFound(task_id.to_string()),
            _ => e,
        })
    }

    async fn update_task(&self, task: Task) -> SessionResult<()> {
        let task_id = task.public_id.clone();
        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE tasks SET kind = ?, status = ?, expected_deliverable = ?, acceptance_criteria = ?, updated_at = ? WHERE public_id = ?",
                params![
                    task.kind.to_string(),
                    task.status.to_string(),
                    task.expected_deliverable,
                    task.acceptance_criteria,
                    OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
                    task.public_id,
                ],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => SessionError::TaskNotFound(task_id),
            _ => e,
        })
    }

    async fn delete_task(&self, task_id: &str) -> SessionResult<()> {
        let task_id_str = task_id.to_string();
        let affected = self
            .run_blocking(move |conn| {
                conn.execute(
                    "DELETE FROM tasks WHERE public_id = ?",
                    params![task_id_str],
                )
            })
            .await?;

        if affected == 0 {
            return Err(SessionError::TaskNotFound(task_id.to_string()));
        }
        Ok(())
    }
}
