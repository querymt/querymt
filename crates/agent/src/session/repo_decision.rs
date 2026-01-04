//! SQLite implementation of DecisionRepository

use crate::session::domain::{Alternative, AlternativeStatus, Decision, DecisionStatus};
use crate::session::error::{SessionError, SessionResult};
use crate::session::repository::DecisionRepository;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// SQLite implementation of DecisionRepository
#[derive(Clone)]
pub struct SqliteDecisionRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDecisionRepository {
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

impl fmt::Display for DecisionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecisionStatus::Accepted => write!(f, "accepted"),
            DecisionStatus::Rejected => write!(f, "rejected"),
        }
    }
}

impl FromStr for DecisionStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "accepted" => Ok(DecisionStatus::Accepted),
            "rejected" => Ok(DecisionStatus::Rejected),
            _ => Err(format!("Unknown decision status: {}", s)),
        }
    }
}

impl fmt::Display for AlternativeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlternativeStatus::Active => write!(f, "active"),
            AlternativeStatus::Discarded => write!(f, "discarded"),
        }
    }
}

impl FromStr for AlternativeStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(AlternativeStatus::Active),
            "discarded" => Ok(AlternativeStatus::Discarded),
            _ => Err(format!("Unknown alternative status: {}", s)),
        }
    }
}

fn map_row_to_decision(row: &Row) -> rusqlite::Result<Decision> {
    let status_str: String = row.get(5)?;
    let created_at_str: String = row.get(6)?;
    Ok(Decision {
        id: row.get(0)?,
        session_id: row.get(1)?,
        task_id: row.get(2)?,
        description: row.get(3)?,
        rationale: row.get(4)?,
        status: DecisionStatus::from_str(&status_str).map_err(|_| rusqlite::Error::InvalidQuery)?,
        created_at: OffsetDateTime::parse(
            &created_at_str,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

fn map_row_to_alternative(row: &Row) -> rusqlite::Result<Alternative> {
    let status_str: String = row.get(4)?;
    let created_at_str: String = row.get(5)?;
    Ok(Alternative {
        id: row.get(0)?,
        session_id: row.get(1)?,
        task_id: row.get(2)?,
        description: row.get(3)?,
        status: AlternativeStatus::from_str(&status_str)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        created_at: OffsetDateTime::parse(
            &created_at_str,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

#[async_trait]
impl DecisionRepository for SqliteDecisionRepository {
    async fn record_decision(&self, decision: Decision) -> SessionResult<()> {
        let Decision {
            session_id,
            task_id,
            description,
            rationale,
            status,
            created_at,
            ..
        } = decision;
        let status_str = status.to_string();
        let created_at_str = created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO decisions (session_id, task_id, description, rationale, status, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    session_id,
                    task_id,
                    description,
                    rationale,
                    status_str,
                    created_at_str,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn record_alternative(&self, alternative: Alternative) -> SessionResult<()> {
        let Alternative {
            session_id,
            task_id,
            description,
            status,
            created_at,
            ..
        } = alternative;
        let status_str = status.to_string();
        let created_at_str = created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO alternatives (session_id, task_id, description, status, created_at) VALUES (?, ?, ?, ?, ?)",
                params![session_id, task_id, description, status_str, created_at_str],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_decision(&self, decision_id: &str) -> SessionResult<Option<Decision>> {
        let decision_id_owned = decision_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, session_id, task_id, description, rationale, status, created_at FROM decisions WHERE id = ?",
                params![decision_id_owned],
                map_row_to_decision,
            )
            .optional()
        })
        .await
    }

    async fn list_decisions(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Decision>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        let internal_task_id = if let Some(tid) = task_id {
            Some(self.resolve_task_internal_id(tid).await?)
        } else {
            None
        };

        self.run_blocking(move |conn| {
            if let Some(task) = internal_task_id {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, description, rationale, status, created_at FROM decisions WHERE session_id = ? AND task_id = ? ORDER BY created_at ASC",
                )?;
                let decisions_iter = stmt.query_map(
                    params![internal_session_id, task],
                    map_row_to_decision,
                )?;
                decisions_iter.collect::<Result<Vec<_>, _>>()
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, description, rationale, status, created_at FROM decisions WHERE session_id = ? ORDER BY created_at ASC",
                )?;
                let decisions_iter = stmt.query_map(
                    params![internal_session_id],
                    map_row_to_decision,
                )?;
                decisions_iter.collect::<Result<Vec<_>, _>>()
            }
        })
        .await
    }

    async fn list_alternatives(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Alternative>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        let internal_task_id = if let Some(tid) = task_id {
            Some(self.resolve_task_internal_id(tid).await?)
        } else {
            None
        };

        self.run_blocking(move |conn| {
            if let Some(task) = internal_task_id {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, description, status, created_at FROM alternatives WHERE session_id = ? AND task_id = ? ORDER BY created_at ASC",
                )?;
                let alternatives_iter = stmt.query_map(
                    params![internal_session_id, task],
                    map_row_to_alternative,
                )?;
                alternatives_iter.collect::<Result<Vec<_>, _>>()
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, description, status, created_at FROM alternatives WHERE session_id = ? ORDER BY created_at ASC",
                )?;
                let alternatives_iter = stmt.query_map(
                    params![internal_session_id],
                    map_row_to_alternative,
                )?;
                alternatives_iter.collect::<Result<Vec<_>, _>>()
            }
        })
        .await
    }

    async fn update_decision_status(
        &self,
        decision_id: &str,
        status: DecisionStatus,
    ) -> SessionResult<()> {
        let decision_id_owned = decision_id.to_string();
        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE decisions SET status = ? WHERE id = ?",
                params![status.to_string(), decision_id_owned],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => {
                SessionError::DecisionNotFound(decision_id.to_string())
            }
            other => other,
        })
    }

    async fn update_alternative_status(
        &self,
        alternative_id: &str,
        status: AlternativeStatus,
    ) -> SessionResult<()> {
        let alternative_id_owned = alternative_id.to_string();
        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE alternatives SET status = ? WHERE id = ?",
                params![status.to_string(), alternative_id_owned],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => {
                SessionError::AlternativeNotFound(alternative_id.to_string())
            }
            other => other,
        })
    }
}
