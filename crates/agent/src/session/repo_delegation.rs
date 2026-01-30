//! SQLite implementation of DelegationRepository

use crate::session::domain::{Delegation, DelegationStatus};
use crate::session::error::{SessionError, SessionResult};
use crate::session::repository::DelegationRepository;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

/// SQLite implementation of DelegationRepository
#[derive(Clone)]
pub struct SqliteDelegationRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDelegationRepository {
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
        match self
            .run_blocking(move |conn| {
                conn.query_row(
                    "SELECT id FROM sessions WHERE public_id = ?",
                    params![query_value],
                    |row| row.get(0),
                )
            })
            .await
        {
            Ok(id) => Ok(id),
            Err(SessionError::DatabaseError(msg)) if msg.contains("Query returned no rows") => {
                Err(SessionError::SessionNotFound(error_value))
            }
            Err(e) => Err(e),
        }
    }
}

impl fmt::Display for DelegationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DelegationStatus::Requested => write!(f, "requested"),
            DelegationStatus::Running => write!(f, "running"),
            DelegationStatus::Complete => write!(f, "complete"),
            DelegationStatus::Failed => write!(f, "failed"),
            DelegationStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl FromStr for DelegationStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "requested" => Ok(DelegationStatus::Requested),
            "running" => Ok(DelegationStatus::Running),
            "complete" => Ok(DelegationStatus::Complete),
            "failed" => Ok(DelegationStatus::Failed),
            "cancelled" => Ok(DelegationStatus::Cancelled),
            _ => Err(format!("Unknown delegation status: {}", s)),
        }
    }
}

#[async_trait]
impl DelegationRepository for SqliteDelegationRepository {
    async fn create_delegation(&self, mut delegation: Delegation) -> SessionResult<Delegation> {
        if delegation.public_id.trim().is_empty() {
            delegation.public_id = Uuid::now_v7().to_string();
        }
        let public_id = delegation.public_id.clone();
        let created_at = format_rfc3339(&delegation.created_at);
        let completed_at = delegation.completed_at.as_ref().map(format_rfc3339);
        let session_id = delegation.session_id;
        let task_id = delegation.task_id;
        let target_agent = delegation.target_agent_id.clone();
        let objective = delegation.objective.clone();
        // NOTE: Store hash as TEXT (hex) for now - could switch to BLOB for efficiency
        let objective_hash = delegation.objective_hash.to_hex();
        let context = delegation.context.clone();
        let constraints = delegation.constraints.clone();
        let expected_output = delegation.expected_output.clone();
        let status_str = delegation.status.to_string();
        let retry_count = delegation.retry_count;

        let internal_id = self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO delegations (public_id, session_id, task_id, target_agent_id, objective, objective_hash, context, constraints, expected_output, status, retry_count, created_at, completed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    public_id,
                    session_id,
                    task_id,
                    target_agent,
                    objective,
                    objective_hash,
                    context,
                    constraints,
                    expected_output,
                    status_str,
                    retry_count,
                    created_at,
                    completed_at,
                ],
            )?;
            Ok::<i64, rusqlite::Error>(conn.last_insert_rowid())
        })
        .await?;

        delegation.id = internal_id;
        Ok(delegation)
    }

    async fn get_delegation(&self, delegation_id: &str) -> SessionResult<Option<Delegation>> {
        let delegation_id = delegation_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, public_id, session_id, task_id, target_agent_id, objective, objective_hash, context, constraints, expected_output, status, retry_count, created_at, completed_at FROM delegations WHERE public_id = ?",
                params![delegation_id],
                map_row_to_delegation,
            )
            .optional()
        })
        .await
    }

    async fn list_delegations(&self, session_id: &str) -> SessionResult<Vec<Delegation>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, public_id, session_id, task_id, target_agent_id, objective, objective_hash, context, constraints, expected_output, status, retry_count, created_at, completed_at FROM delegations WHERE session_id = ? ORDER BY created_at ASC",
            )?;
            let delegations_iter = stmt.query_map(params![internal_session_id], map_row_to_delegation)?;
            delegations_iter.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn update_delegation_status(
        &self,
        delegation_id: &str,
        status: DelegationStatus,
    ) -> SessionResult<()> {
        let delegation_id_owned = delegation_id.to_string();
        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE delegations SET status = ? WHERE public_id = ?",
                params![status.to_string(), delegation_id_owned],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => {
                SessionError::DelegationNotFound(delegation_id.to_string())
            }
            _ => e,
        })
    }

    async fn update_delegation(&self, delegation: Delegation) -> SessionResult<()> {
        let status_str = delegation.status.to_string();
        let completed_at = delegation.completed_at.as_ref().map(format_rfc3339);
        let delegation_id = delegation.public_id.clone();
        // NOTE: Store hash as TEXT (hex) for now - could switch to BLOB for efficiency
        let objective_hash = delegation.objective_hash.to_hex();

        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE delegations SET target_agent_id = ?, objective = ?, objective_hash = ?, context = ?, constraints = ?, expected_output = ?, status = ?, retry_count = ?, completed_at = ? WHERE public_id = ?",
                params![
                    delegation.target_agent_id,
                    delegation.objective,
                    objective_hash,
                    delegation.context,
                    delegation.constraints,
                    delegation.expected_output,
                    status_str,
                    delegation.retry_count,
                    completed_at,
                    delegation_id,
                ],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            SessionError::DatabaseError(_) => {
                SessionError::DelegationNotFound(delegation.public_id)
            }
            _ => e,
        })
    }
}

fn format_rfc3339(dt: &OffsetDateTime) -> String {
    dt.format(&Rfc3339).unwrap_or_default()
}

fn parse_rfc3339(value: &str) -> Result<OffsetDateTime, rusqlite::Error> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn map_row_to_delegation(row: &rusqlite::Row) -> Result<Delegation, rusqlite::Error> {
    let status_str: String = row.get(10)?;
    let created_at = parse_rfc3339(&row.get::<_, String>(12)?)?;
    let completed_at = row
        .get::<_, Option<String>>(13)?
        .and_then(|s| OffsetDateTime::parse(&s, &Rfc3339).ok());

    // NOTE: Parse hash from TEXT (hex) - could switch to BLOB for efficiency
    let objective_hash_str: String = row.get(6)?;
    let objective_hash = crate::hash::RapidHash::from_hex(&objective_hash_str)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(Delegation {
        id: row.get(0)?,
        public_id: row.get(1)?,
        session_id: row.get(2)?,
        task_id: row.get(3)?,
        target_agent_id: row.get(4)?,
        objective: row.get(5)?,
        objective_hash,
        context: row.get(7)?,
        constraints: row.get(8)?,
        expected_output: row.get(9)?,
        verification_spec: None,
        status: DelegationStatus::from_str(&status_str)
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
        retry_count: row.get::<_, i64>(11)? as u32,
        created_at,
        completed_at,
    })
}
