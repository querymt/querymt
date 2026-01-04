//! SQLite implementation of ArtifactRepository

use crate::session::domain::Artifact;
use crate::session::error::{SessionError, SessionResult};
use crate::session::repository::ArtifactRepository;
use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// SQLite implementation of ArtifactRepository
#[derive(Clone)]
pub struct SqliteArtifactRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteArtifactRepository {
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

fn map_row_to_artifact(row: &Row) -> rusqlite::Result<Artifact> {
    let created_at_str: String = row.get(7)?;
    Ok(Artifact {
        id: row.get(0)?,
        session_id: row.get(1)?,
        task_id: row.get(2)?,
        kind: row.get(3)?,
        uri: row.get(4)?,
        path: row.get(5)?,
        summary: row.get(6)?,
        created_at: OffsetDateTime::parse(
            &created_at_str,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)?,
    })
}

#[async_trait]
impl ArtifactRepository for SqliteArtifactRepository {
    async fn record_artifact(&self, artifact: Artifact) -> SessionResult<()> {
        let Artifact {
            session_id,
            task_id,
            kind,
            uri,
            path,
            summary,
            created_at,
            ..
        } = artifact;
        let created_at_str = created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();

        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO artifacts (session_id, task_id, kind, uri, path, summary, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    session_id,
                    task_id,
                    kind,
                    uri,
                    path,
                    summary,
                    created_at_str,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_artifact(&self, artifact_id: &str) -> SessionResult<Option<Artifact>> {
        let artifact_id_owned = artifact_id.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT id, session_id, task_id, kind, uri, path, summary, created_at FROM artifacts WHERE id = ?",
                params![artifact_id_owned],
                map_row_to_artifact,
            )
            .optional()
        })
        .await
    }

    async fn list_artifacts(
        &self,
        session_id: &str,
        task_id: Option<&str>,
    ) -> SessionResult<Vec<Artifact>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        let internal_task_id = if let Some(tid) = task_id {
            Some(self.resolve_task_internal_id(tid).await?)
        } else {
            None
        };

        self.run_blocking(move |conn| {
            if let Some(task) = internal_task_id {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, kind, uri, path, summary, created_at FROM artifacts WHERE session_id = ? AND task_id = ? ORDER BY created_at ASC",
                )?;
                let artifacts_iter = stmt.query_map(
                    params![internal_session_id, task],
                    map_row_to_artifact,
                )?;
                artifacts_iter.collect::<Result<Vec<_>, _>>()
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, session_id, task_id, kind, uri, path, summary, created_at FROM artifacts WHERE session_id = ? ORDER BY created_at ASC",
                )?;
                let artifacts_iter = stmt.query_map(
                    params![internal_session_id],
                    map_row_to_artifact,
                )?;
                artifacts_iter.collect::<Result<Vec<_>, _>>()
            }
        })
        .await
    }

    async fn list_artifacts_by_kind(
        &self,
        session_id: &str,
        kind: &str,
    ) -> SessionResult<Vec<Artifact>> {
        let internal_session_id = self.resolve_session_internal_id(session_id).await?;
        let kind_owned = kind.to_string();
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, task_id, kind, uri, path, summary, created_at FROM artifacts WHERE session_id = ? AND kind = ? ORDER BY created_at ASC",
            )?;
            let artifacts_iter = stmt.query_map(
                params![internal_session_id, kind_owned],
                map_row_to_artifact,
            )?;
            artifacts_iter.collect::<Result<Vec<_>, _>>()
        })
        .await
    }
}
