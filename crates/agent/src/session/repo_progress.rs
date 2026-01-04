//! SQLite implementation of ProgressRepository

use crate::session::domain::{ProgressEntry, ProgressKind};
use crate::session::error::{SessionError, SessionResult};
use crate::session::repository::ProgressRepository;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// SQLite implementation of ProgressRepository
#[derive(Clone)]
pub struct SqliteProgressRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteProgressRepository {
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

    async fn resolve_session_internal_id(&self, session_public_id: &str) -> SessionResult<i64> {
        let query_value = session_public_id.to_string();
        let error_value = query_value.clone();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id FROM sessions WHERE public_id = ?",
                params![query_value],
                |row| row.get(0),
            )
        })
        .await
        .map_err(|err| match err {
            SessionError::DatabaseError(msg) if msg.contains("Query returned no rows") => {
                SessionError::SessionNotFound(error_value)
            }
            other => other,
        })
    }

    async fn resolve_task_internal_id(&self, task_public_id: &str) -> SessionResult<i64> {
        let query_value = task_public_id.to_string();
        let error_value = query_value.clone();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id FROM tasks WHERE public_id = ?",
                params![query_value],
                |row| row.get(0),
            )
        })
        .await
        .map_err(|err| match err {
            SessionError::DatabaseError(msg) if msg.contains("Query returned no rows") => {
                SessionError::TaskNotFound(error_value)
            }
            other => other,
        })
    }
}

impl fmt::Display for ProgressKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProgressKind::ToolCall => write!(f, "tool_call"),
            ProgressKind::Artifact => write!(f, "artifact"),
            ProgressKind::Note => write!(f, "note"),
            ProgressKind::Checkpoint => write!(f, "checkpoint"),
        }
    }
}

impl FromStr for ProgressKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tool_call" => Ok(ProgressKind::ToolCall),
            "artifact" => Ok(ProgressKind::Artifact),
            "note" => Ok(ProgressKind::Note),
            "checkpoint" => Ok(ProgressKind::Checkpoint),
            _ => Err(format!("Unknown progress kind: {}", s)),
        }
    }
}

fn map_row_to_progress_entry(row: &Row) -> rusqlite::Result<ProgressEntry> {
    let kind_str: String = row.get(3)?;
    let created_at_str: String = row.get(6)?;
    Ok(ProgressEntry {
        id: row.get(0)?,
        session_id: row.get(1)?,
        task_id: row.get(2)?,
        kind: ProgressKind::from_str(&kind_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
        content: row.get(4)?,
        metadata: row.get(5)?,
        created_at: OffsetDateTime::parse(
            &created_at_str,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

#[async_trait]
impl ProgressRepository for SqliteProgressRepository {
    async fn append_progress_entry(&self, entry: ProgressEntry) -> SessionResult<()> {
        let ProgressEntry {
            session_id,
            task_id,
            kind,
            content,
            metadata,
            created_at,
            ..
        } = entry;
        let kind_str = kind.to_string();
        let created_at_str = created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO progress_entries (session_id, task_id, kind, content, metadata, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                params![session_id, task_id, kind_str, content, metadata, created_at_str],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_progress_entry(&self, entry_id: &str) -> SessionResult<Option<ProgressEntry>> {
        let entry_id_owned = entry_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, session_id, task_id, kind, content, metadata, created_at FROM progress_entries WHERE id = ?",
                params![entry_id_owned],
                map_row_to_progress_entry,
            )
            .optional()
        })
        .await
    }

    async fn list_progress_entries(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<ProgressEntry>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        let internal_task_id = if let Some(tid) = task_id {
            Some(self.resolve_task_internal_id(tid).await?)
        } else {
            None
        };

        self.run_blocking(move |conn| {
            if let Some(task) = internal_task_id {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, kind, content, metadata, created_at FROM progress_entries WHERE session_id = ? AND task_id = ? ORDER BY created_at ASC",
                )?;
                let entries_iter = stmt.query_map(
                    params![internal_session_id, task],
                    map_row_to_progress_entry,
                )?;
                entries_iter.collect::<Result<Vec<_>, _>>()
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, kind, content, metadata, created_at FROM progress_entries WHERE session_id = ? ORDER BY created_at ASC",
                )?;
                let entries_iter = stmt.query_map(
                    params![internal_session_id],
                    map_row_to_progress_entry,
                )?;
                entries_iter.collect::<Result<Vec<_>, _>>()
            }
        })
        .await
    }

    async fn list_progress_by_kind(
        &self,
        session_id: &str,
        kind: ProgressKind,
    ) -> SessionResult<Vec<ProgressEntry>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        let kind_str = kind.to_string();
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, task_id, kind, content, metadata, created_at FROM progress_entries WHERE session_id = ? AND kind = ? ORDER BY created_at ASC",
            )?;
            let entries_iter = stmt.query_map(
                params![internal_session_id, kind_str],
                map_row_to_progress_entry,
            )?;
            entries_iter.collect::<Result<Vec<_>, _>>()
        })
        .await
    }
}
