//! SQLite-backed automation binding repository.

use async_trait::async_trait;
use parking_lot::Mutex;
use querymt_agent_core::dotagents_automation::{
    AUTOMATION_SESSION_KIND, DotagentsAutomationIdentity, DotagentsAutomationRepository,
};
use querymt_agent_core::dotagents_tasks::DotagentsAutomationBinding;
use querymt_agent_core::session::error::SessionResult;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::Arc;
use time::OffsetDateTime;

/// SQLite-backed automation binding repository.
pub struct SqliteDotagentsAutomationRepository {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteDotagentsAutomationRepository {
    /// Create a repository over an existing connection.
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

/// Row shape for automation bindings.
type BindingRow = (String, String, Option<String>, Option<String>, String);

fn map_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<BindingRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

#[async_trait]
impl DotagentsAutomationRepository for SqliteDotagentsAutomationRepository {
    async fn ensure_automation_session(
        &self,
        identity: &DotagentsAutomationIdentity,
    ) -> SessionResult<DotagentsAutomationBinding> {
        let binding_key = identity.binding_key();
        let layer = identity.layer;
        let canonical_workspace = identity
            .canonical_workspace
            .as_ref()
            .map(|path| path.to_string_lossy().to_string());
        let profile_id = identity.profile_id.clone();
        let session_name = identity.session_name();
        let now = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        // The `list_runs`-style join below returns an active session only; a
        // softly deleted session means the scope must be re-provisioned.
        let session_public_id = uuid::Uuid::now_v7().to_string();

        self.run_blocking(move |conn| {
            let tx = conn.transaction()?;

            // Reuse an existing binding when its session still exists. The
            // foreign key uses ON DELETE CASCADE, so a surviving row implies a
            // surviving session, but the join keeps the check explicit.
            let existing: Option<String> = tx
                .query_row(
                    "SELECT b.session_public_id FROM dotagents_automation_bindings b \
                     JOIN sessions s ON s.public_id = b.session_public_id \
                     WHERE b.binding_key = ?1",
                    params![binding_key],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(session_public_id) = existing {
                tx.commit()?;
                return Ok(DotagentsAutomationBinding::new(
                    session_public_id,
                    profile_id,
                ));
            }

            tx.execute(
                "INSERT INTO sessions (
                    public_id, name, cwd, created_at, updated_at, session_kind
                 ) VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![
                    session_public_id,
                    session_name,
                    canonical_workspace,
                    now,
                    AUTOMATION_SESSION_KIND,
                ],
            )?;

            tx.execute(
                "INSERT INTO dotagents_automation_bindings (
                    binding_key, layer, canonical_workspace, profile_id,
                    session_public_id, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(binding_key) DO UPDATE SET
                    profile_id = excluded.profile_id,
                    session_public_id = excluded.session_public_id,
                    updated_at = excluded.updated_at",
                params![
                    binding_key,
                    layer.as_str(),
                    canonical_workspace,
                    profile_id,
                    session_public_id,
                    now,
                ],
            )?;

            tx.commit()?;
            Ok(DotagentsAutomationBinding::new(
                session_public_id,
                profile_id,
            ))
        })
        .await
    }

    async fn find_automation_session(
        &self,
        identity: &DotagentsAutomationIdentity,
    ) -> SessionResult<Option<DotagentsAutomationBinding>> {
        let binding_key = identity.binding_key();

        self.run_blocking(move |conn| {
            let row: Option<BindingRow> = conn
                .query_row(
                    "SELECT binding_key, layer, canonical_workspace, profile_id, session_public_id \
                     FROM dotagents_automation_bindings WHERE binding_key = ?1",
                    params![binding_key],
                    map_binding,
                )
                .optional()?;
            match row {
                Some(_) => {
                    let session_public_id: String = conn.query_row(
                        "SELECT session_public_id FROM dotagents_automation_bindings \
                         WHERE binding_key = ?1",
                        params![binding_key],
                        |row| row.get(0),
                    )?;
                    let profile_id: Option<String> = conn.query_row(
                        "SELECT profile_id FROM dotagents_automation_bindings \
                         WHERE binding_key = ?1",
                        params![binding_key],
                        |row| row.get(0),
                    )?;
                    Ok(Some(DotagentsAutomationBinding::new(
                        session_public_id,
                        profile_id,
                    )))
                }
                None => Ok(None),
            }
        })
        .await
    }
}
