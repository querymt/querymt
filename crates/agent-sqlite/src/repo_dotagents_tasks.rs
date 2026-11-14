//! SQLite implementation of protocol task ownership and trust persistence.

use async_trait::async_trait;
use parking_lot::Mutex;
use querymt_agent_core::dotagents::layer::DotagentsLayer;
use querymt_agent_core::dotagents_tasks::{
    DotagentsTaskApproval, DotagentsTaskApprovalDecision, DotagentsTaskOwnership,
    DotagentsTaskStateRepository, normalize_id,
};
use querymt_agent_core::session::error::SessionResult;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::OffsetDateTime;

/// SQLite implementation of protocol task ownership and trust persistence.
pub struct SqliteDotagentsTaskStateRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDotagentsTaskStateRepository {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    async fn run_blocking<F, R>(&self, f: F) -> SessionResult<R>
    where
        F: FnOnce(&mut Connection) -> Result<R, rusqlite::Error> + Send + 'static,
        R: Send + 'static,
    {
        crate::session::sqlite_storage::blocking::with_conn_sqlite(&self.conn, f).await
    }
}

#[async_trait]
impl DotagentsTaskStateRepository for SqliteDotagentsTaskStateRepository {
    async fn upsert_ownership(&self, ownership: DotagentsTaskOwnership) -> SessionResult<()> {
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO dotagents_task_ownership (
                    source_key, task_creation_key, schedule_creation_key, layer,
                    canonical_workspace, task_id, fingerprint, task_public_id,
                    schedule_public_id, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                 ON CONFLICT(source_key) DO UPDATE SET
                    fingerprint = excluded.fingerprint,
                    task_public_id = excluded.task_public_id,
                    schedule_public_id = excluded.schedule_public_id,
                    updated_at = excluded.updated_at",
                params![
                    ownership.source_key,
                    ownership.task_creation_key,
                    ownership.schedule_creation_key,
                    ownership.layer.as_str(),
                    ownership
                        .canonical_workspace
                        .as_deref()
                        .map(|path| path.to_string_lossy().into_owned()),
                    ownership.task_id,
                    ownership.fingerprint,
                    ownership.task_public_id,
                    ownership.schedule_public_id,
                    format_time(OffsetDateTime::now_utc()),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_ownership(
        &self,
        source_key: &str,
    ) -> SessionResult<Option<DotagentsTaskOwnership>> {
        let source_key = source_key.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT source_key, task_creation_key, schedule_creation_key, layer,
                        canonical_workspace, task_id, fingerprint, task_public_id,
                        schedule_public_id
                 FROM dotagents_task_ownership WHERE source_key = ?1",
                [source_key],
                map_ownership,
            )
            .optional()
        })
        .await
    }

    async fn list_ownerships(&self) -> SessionResult<Vec<DotagentsTaskOwnership>> {
        self.run_blocking(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT source_key, task_creation_key, schedule_creation_key, layer,
                        canonical_workspace, task_id, fingerprint, task_public_id,
                        schedule_public_id
                 FROM dotagents_task_ownership ORDER BY source_key ASC",
            )?;
            let rows = stmt.query_map([], map_ownership)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
    }

    async fn delete_ownership(&self, source_key: &str) -> SessionResult<()> {
        let source_key = source_key.to_string();
        self.run_blocking(move |conn| {
            conn.execute(
                "DELETE FROM dotagents_task_ownership WHERE source_key = ?1",
                [source_key],
            )?;
            Ok(())
        })
        .await
    }

    async fn record_approval(&self, approval: DotagentsTaskApproval) -> SessionResult<()> {
        self.run_blocking(move |conn| {
            conn.execute(
                "INSERT INTO dotagents_task_approvals (
                    canonical_workspace, task_id, fingerprint, decision, decided_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(canonical_workspace, task_id, fingerprint) DO UPDATE SET
                    decision = excluded.decision, decided_at = excluded.decided_at",
                params![
                    approval.canonical_workspace.to_string_lossy(),
                    normalize_id(&approval.task_id),
                    approval.fingerprint,
                    approval.decision.as_str(),
                    format_time(approval.decided_at),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn get_approval(
        &self,
        canonical_workspace: &Path,
        task_id: &str,
        fingerprint: &str,
    ) -> SessionResult<Option<DotagentsTaskApproval>> {
        let workspace = canonical_workspace.to_string_lossy().into_owned();
        let task_id = normalize_id(task_id);
        let fingerprint = fingerprint.to_string();
        self.run_blocking(move |conn| {
            conn.query_row(
                "SELECT canonical_workspace, task_id, fingerprint, decision, decided_at
                 FROM dotagents_task_approvals
                 WHERE canonical_workspace = ?1 AND task_id = ?2 AND fingerprint = ?3",
                params![workspace, task_id, fingerprint],
                map_approval,
            )
            .optional()
        })
        .await
    }
}

fn map_ownership(row: &rusqlite::Row<'_>) -> rusqlite::Result<DotagentsTaskOwnership> {
    let layer: String = row.get(3)?;
    Ok(DotagentsTaskOwnership {
        source_key: row.get(0)?,
        task_creation_key: row.get(1)?,
        schedule_creation_key: row.get(2)?,
        layer: match layer.as_str() {
            "global" => DotagentsLayer::Global,
            "workspace" => DotagentsLayer::Workspace,
            _ => return Err(rusqlite::Error::InvalidQuery),
        },
        canonical_workspace: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
        task_id: row.get(5)?,
        fingerprint: row.get(6)?,
        task_public_id: row.get(7)?,
        schedule_public_id: row.get(8)?,
    })
}

fn map_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<DotagentsTaskApproval> {
    let decision: String = row.get(3)?;
    let decided_at: String = row.get(4)?;
    Ok(DotagentsTaskApproval {
        canonical_workspace: PathBuf::from(row.get::<_, String>(0)?),
        task_id: row.get(1)?,
        fingerprint: row.get(2)?,
        decision: DotagentsTaskApprovalDecision::parse(&decision)
            .ok_or(rusqlite::Error::InvalidQuery)?,
        decided_at: OffsetDateTime::parse(
            &decided_at,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

fn format_time(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
