//! SQLite implementation of SessionRepository

use crate::session::domain::{ForkInfo, ForkOrigin, ForkPointType};
use crate::session::error::{SessionError, SessionResult};
use crate::session::repository::SessionRepository;
use crate::session::store::Session;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::{Arc, Mutex};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

/// SQLite implementation of SessionRepository
#[derive(Clone)]
pub struct SqliteSessionRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteSessionRepository {
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

    async fn resolve_task_internal_id(&self, task_public_id: &str) -> SessionResult<i64> {
        let query_value = task_public_id.to_string();
        let error_value = query_value.clone();
        match self
            .run_blocking(move |conn| {
                conn.query_row(
                    "SELECT id FROM tasks WHERE public_id = ?",
                    params![query_value],
                    |row| row.get(0),
                )
            })
            .await
        {
            Ok(id) => Ok(id),
            Err(SessionError::DatabaseError(msg)) if msg.contains("Query returned no rows") => {
                Err(SessionError::TaskNotFound(error_value))
            }
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl SessionRepository for SqliteSessionRepository {
    async fn create_session(
        &self,
        name: Option<String>,
        cwd: Option<std::path::PathBuf>,
    ) -> SessionResult<Session> {
        let now = OffsetDateTime::now_utc();
        let now_str = format_rfc3339(&now);
        let public_id = Uuid::now_v7().to_string();
        let public_id_for_insert = public_id.clone();
        let name_for_insert = name.clone();
        let cwd_for_insert = cwd.as_ref().map(|p| p.to_string_lossy().to_string());
        let cwd_clone = cwd.clone();

        let inserted_id = self
            .run_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO sessions (public_id, name, cwd, created_at, updated_at, current_intent_snapshot_id, active_task_id, llm_config_id, parent_session_id, fork_origin, fork_point_type, fork_point_ref, fork_instructions) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        public_id_for_insert,
                        name_for_insert,
                        cwd_for_insert,
                        now_str,
                        now_str,
                        Option::<i64>::None,
                        Option::<i64>::None,
                        Option::<i64>::None,
                        Option::<i64>::None,
                        Option::<String>::None,
                        Option::<String>::None,
                        Option::<String>::None,
                        Option::<String>::None,
                    ],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;

        Ok(Session {
            id: inserted_id,
            public_id,
            name,
            cwd: cwd_clone,
            created_at: Some(now),
            updated_at: Some(now),
            current_intent_snapshot_id: None,
            active_task_id: None,
            llm_config_id: None,
            parent_session_id: None,
            fork_origin: None,
            fork_point_type: None,
            fork_point_ref: None,
            fork_instructions: None,
        })
    }

    async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>> {
        let session_id = session_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, public_id, name, cwd, created_at, updated_at, current_intent_snapshot_id, active_task_id, llm_config_id, parent_session_id, fork_origin, fork_point_type, fork_point_ref, fork_instructions FROM sessions WHERE public_id = ?",
                params![session_id],
                map_row_to_session,
            )
            .optional()
        })
        .await
    }

    async fn list_sessions(&self) -> SessionResult<Vec<Session>> {
        self.run_blocking(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, public_id, name, cwd, created_at, updated_at, current_intent_snapshot_id, active_task_id, llm_config_id, parent_session_id, fork_origin, fork_point_type, fork_point_ref, fork_instructions FROM sessions ORDER BY updated_at DESC",
            )?;
            let sessions_iter = stmt.query_map([], map_row_to_session)?;
            sessions_iter.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn delete_session(&self, session_id: &str) -> SessionResult<()> {
        let session_id = session_id.to_string();
        let session_id_for_delete = session_id.clone();
        let affected = self
            .run_blocking(move |conn| {
                conn.execute(
                    "DELETE FROM sessions WHERE public_id = ?",
                    params![session_id_for_delete],
                )
            })
            .await?;

        if affected == 0 {
            return Err(SessionError::SessionNotFound(session_id));
        }
        Ok(())
    }

    async fn set_current_intent_snapshot(
        &self,
        session_id: &str,
        snapshot_id: Option<&str>,
    ) -> SessionResult<()> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let snapshot_internal_id = parse_optional_numeric_id(snapshot_id, "snapshot")?;
        let updated_at = format_rfc3339(&OffsetDateTime::now_utc());

        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE sessions SET current_intent_snapshot_id = ?, updated_at = ? WHERE id = ?",
                params![snapshot_internal_id, updated_at, session_internal_id],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
    }

    async fn set_active_task(&self, session_id: &str, task_id: Option<&str>) -> SessionResult<()> {
        let session_internal_id = self.resolve_session_internal_id(session_id).await?;
        let internal_task_id = if let Some(task_id) = task_id {
            Some(self.resolve_task_internal_id(task_id).await?)
        } else {
            None
        };
        let updated_at = format_rfc3339(&OffsetDateTime::now_utc());

        self.run_blocking(move |conn| {
            let affected = conn.execute(
                "UPDATE sessions SET active_task_id = ?, updated_at = ? WHERE id = ?",
                params![internal_task_id, updated_at, session_internal_id],
            )?;
            if affected == 0 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(())
        })
        .await
    }

    async fn fork_session(
        &self,
        parent_id: &str,
        fork_point_type: ForkPointType,
        fork_point_ref: &str,
        fork_origin: ForkOrigin,
        additional_instructions: Option<String>,
    ) -> SessionResult<String> {
        let parent_public_id = parent_id.to_string();
        let parent_session = self
            .get_session(&parent_public_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(parent_public_id.clone()))?;
        let parent_internal_id = parent_session.id;
        let parent_llm_config_id = parent_session.llm_config_id;
        let parent_cwd = parent_session
            .cwd
            .as_ref()
            .map(|p| p.to_string_lossy().to_string());
        let now = OffsetDateTime::now_utc();
        let now_str = format_rfc3339(&now);
        let fork_name = format!("Fork of {}", parent_public_id);
        let new_public_id = Uuid::now_v7().to_string();
        let new_public_id_for_insert = new_public_id.clone();
        let fork_point_ref_str = fork_point_ref.to_string();
        let fork_origin_str = fork_origin.to_string();
        let fork_point_type_str = fork_point_type.to_string();
        let instructions_for_insert = additional_instructions;

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO sessions (public_id, name, cwd, created_at, updated_at, llm_config_id, parent_session_id, fork_origin, fork_point_type, fork_point_ref, fork_instructions) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    new_public_id_for_insert,
                    fork_name,
                    parent_cwd,
                    now_str,
                    now_str,
                    parent_llm_config_id,
                    parent_internal_id,
                    fork_origin_str,
                    fork_point_type_str,
                    fork_point_ref_str,
                    instructions_for_insert,
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
        Ok(new_public_id)
    }

    async fn get_session_fork_info(&self, session_id: &str) -> SessionResult<Option<ForkInfo>> {
        if let Some(session) = self.get_session(session_id).await? {
            if session.parent_session_id.is_none() {
                return Ok(None);
            }
            Ok(Some(ForkInfo {
                parent_session_id: session.parent_session_id,
                fork_origin: session.fork_origin,
                fork_point_type: session.fork_point_type,
                fork_point_ref: session.fork_point_ref,
                fork_instructions: session.fork_instructions,
            }))
        } else {
            Ok(None)
        }
    }

    async fn list_child_sessions(&self, parent_id: &str) -> SessionResult<Vec<String>> {
        let parent_internal_id = self.resolve_session_internal_id(parent_id).await?;
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT public_id FROM sessions WHERE parent_session_id = ? ORDER BY created_at ASC",
            )?;
            let ids_iter = stmt.query_map(params![parent_internal_id], |row| row.get(0))?;
            ids_iter.collect::<Result<Vec<String>, _>>()
        })
        .await
    }
}

fn format_rfc3339(dt: &OffsetDateTime) -> String {
    dt.format(&Rfc3339).unwrap_or_default()
}

fn parse_rfc3339(value: &str) -> Result<OffsetDateTime, rusqlite::Error> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| rusqlite::Error::InvalidQuery)
}

fn map_row_to_session(row: &rusqlite::Row) -> Result<Session, rusqlite::Error> {
    let created_at = parse_rfc3339(&row.get::<_, String>(4)?)?;
    let updated_at = parse_rfc3339(&row.get::<_, String>(5)?)?;

    Ok(Session {
        id: row.get(0)?,
        public_id: row.get(1)?,
        name: row.get(2)?,
        cwd: row
            .get::<_, Option<String>>(3)?
            .map(std::path::PathBuf::from),
        created_at: Some(created_at),
        updated_at: Some(updated_at),
        current_intent_snapshot_id: row.get(6)?,
        active_task_id: row.get(7)?,
        llm_config_id: row.get(8)?,
        parent_session_id: row.get(9)?,
        fork_origin: row
            .get::<_, Option<String>>(10)?
            .and_then(|s| s.parse::<ForkOrigin>().ok()),
        fork_point_type: row
            .get::<_, Option<String>>(11)?
            .and_then(|s| s.parse::<ForkPointType>().ok()),
        fork_point_ref: row.get(12)?,
        fork_instructions: row.get(13)?,
    })
}

fn parse_optional_numeric_id(value: Option<&str>, label: &str) -> SessionResult<Option<i64>> {
    if let Some(value) = value {
        value
            .parse::<i64>()
            .map(Some)
            .map_err(|_| SessionError::InvalidOperation(format!("Invalid {} id: {}", label, value)))
    } else {
        Ok(None)
    }
}
