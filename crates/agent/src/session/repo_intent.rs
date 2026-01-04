//! SQLite implementation of IntentRepository

use crate::session::domain::IntentSnapshot;
use crate::session::error::{SessionError, SessionResult};
use crate::session::repository::IntentRepository;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// SQLite implementation of IntentRepository
#[derive(Clone)]
pub struct SqliteIntentRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteIntentRepository {
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
}

fn map_row_to_snapshot(row: &Row) -> rusqlite::Result<IntentSnapshot> {
    let created_at_str: String = row.get(6)?;
    Ok(IntentSnapshot {
        id: row.get(0)?,
        session_id: row.get(1)?,
        task_id: row.get(2)?,
        summary: row.get(3)?,
        constraints: row.get(4)?,
        next_step_hint: row.get(5)?,
        created_at: OffsetDateTime::parse(
            &created_at_str,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

#[async_trait]
impl IntentRepository for SqliteIntentRepository {
    async fn create_intent_snapshot(&self, snapshot: IntentSnapshot) -> SessionResult<()> {
        let IntentSnapshot {
            session_id,
            task_id,
            summary,
            constraints,
            next_step_hint,
            created_at,
            ..
        } = snapshot;
        let created_at_str = created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO intent_snapshots (session_id, task_id, summary, constraints, next_step_hint, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                params![session_id, task_id, summary, constraints, next_step_hint, created_at_str],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_intent_snapshot(
        &self,
        snapshot_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>> {
        let snapshot_id_owned = snapshot_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, session_id, task_id, summary, constraints, next_step_hint, created_at FROM intent_snapshots WHERE id = ?",
                params![snapshot_id_owned],
                map_row_to_snapshot,
            )
            .optional()
        })
        .await
    }

    async fn list_intent_snapshots(&self, session_id: &str) -> SessionResult<Vec<IntentSnapshot>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, task_id, summary, constraints, next_step_hint, created_at FROM intent_snapshots WHERE session_id = ? ORDER BY created_at ASC",
            )?;
            let snapshots_iter = stmt.query_map(params![internal_session_id], |row| {
                map_row_to_snapshot(row)
            })?;
            snapshots_iter.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn get_current_intent_snapshot(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<IntentSnapshot>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, session_id, task_id, summary, constraints, next_step_hint, created_at FROM intent_snapshots WHERE session_id = ? ORDER BY created_at DESC LIMIT 1",
                params![internal_session_id],
                map_row_to_snapshot,
            )
            .optional()
        })
        .await
    }
}
