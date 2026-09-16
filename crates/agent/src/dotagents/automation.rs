//! Protocol-owned automation sessions.
//!
//! A protocol repeat task is durable configuration: it must survive restarts and
//! reconcile into the *same* task and schedule records each time. QueryMT's
//! scheduler binds a schedule to a session, so protocol reconciliation needs one
//! stable session per protocol scope and execution target.
//!
//! Rather than making every host manufacture and thread a session ID through
//! builder APIs, QueryMT owns these sessions. [`DotagentsAutomationIdentity`]
//! derives a deterministic binding key from the protocol layer, the canonical
//! workspace, and the target profile, and
//! [`DotagentsAutomationRepository::ensure_automation_session`] returns the
//! existing session or creates one exactly once.
//!
//! Sessions created here are tagged with [`AUTOMATION_SESSION_KIND`] so hosts can
//! distinguish protocol-owned automation from user sessions.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

use super::layer::DotagentsLayer;
use super::tasks::DotagentsAutomationBinding;
use crate::session::error::{SessionError, SessionResult};

/// The `sessions.session_kind` value used for protocol automation sessions.
pub const AUTOMATION_SESSION_KIND: &str = "dotagents_automation";

/// Version tag embedded in automation binding keys.
const BINDING_KEY_VERSION: &str = "v1";

/// The identity of a protocol automation scope.
///
/// Two protocol trees that share a layer, canonical workspace, and target
/// profile resolve to the same identity, and therefore to the same durable
/// automation session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DotagentsAutomationIdentity {
    /// The protocol layer the tasks came from.
    pub layer: DotagentsLayer,
    /// The canonical workspace for workspace-layer tasks.
    pub canonical_workspace: Option<PathBuf>,
    /// The target profile that will execute the tasks, when profile-scoped.
    pub profile_id: Option<String>,
}

impl DotagentsAutomationIdentity {
    /// Create an automation identity.
    pub fn new(
        layer: DotagentsLayer,
        canonical_workspace: Option<impl Into<PathBuf>>,
        profile_id: Option<impl Into<String>>,
    ) -> Self {
        Self {
            layer,
            canonical_workspace: canonical_workspace.map(Into::into),
            profile_id: profile_id.map(Into::into),
        }
    }

    /// The deterministic, filesystem-independent binding key.
    ///
    /// Contains no secret material; it is derived only from the protocol layer,
    /// a hash of the canonical workspace path, and the target profile ID.
    pub fn binding_key(&self) -> String {
        let workspace = match &self.canonical_workspace {
            Some(path) => hash_path(path),
            None => "none".to_string(),
        };
        let profile = match &self.profile_id {
            Some(profile_id) => super::merge::normalize_id(profile_id),
            None => "default".to_string(),
        };
        format!(
            "dotagents:{BINDING_KEY_VERSION}:automation:{}:{workspace}:{profile}",
            self.layer.as_str()
        )
    }

    /// A human-readable session name for the automation session.
    pub fn session_name(&self) -> String {
        match (&self.canonical_workspace, &self.profile_id) {
            (Some(workspace), Some(profile_id)) => format!(
                "dotagents automation ({}, {profile_id})",
                workspace.display()
            ),
            (Some(workspace), None) => {
                format!("dotagents automation ({})", workspace.display())
            }
            (None, Some(profile_id)) => format!("dotagents automation (global, {profile_id})"),
            (None, None) => "dotagents automation (global)".to_string(),
        }
    }
}

/// Hash a workspace path into a stable, path-agnostic binding component.
fn hash_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    // A short prefix is plenty for disambiguation and keeps keys readable.
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Creates or reuses the durable session owned by a protocol automation scope.
///
/// Implementations must be idempotent: calling `ensure_automation_session`
/// repeatedly with the same identity returns the same session and never creates
/// a duplicate, so reconciliation after a restart cannot accumulate sessions.
#[async_trait]
pub trait DotagentsAutomationRepository: Send + Sync {
    /// Return the existing automation session for `identity`, creating one if
    /// this scope has never been reconciled.
    async fn ensure_automation_session(
        &self,
        identity: &DotagentsAutomationIdentity,
    ) -> SessionResult<DotagentsAutomationBinding>;

    /// Look up an existing automation binding without creating one.
    async fn find_automation_session(
        &self,
        identity: &DotagentsAutomationIdentity,
    ) -> SessionResult<Option<DotagentsAutomationBinding>>;
}

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
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|error| {
                SessionError::DatabaseError(format!("automation store lock poisoned: {error}"))
            })?;
            f(&mut guard).map_err(SessionError::from)
        })
        .await
        .map_err(|error| {
            SessionError::DatabaseError(format!("automation task join failed: {error}"))
        })?
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

/// Bind the persisted automation session to a profile so scheduler execution
/// routes through the profile runtime rather than the root agent.
pub async fn bind_automation_session_profile(
    store: &dyn crate::session::store::SessionStore,
    session_public_id: &str,
    profile_id: &str,
) -> SessionResult<()> {
    store
        .set_profile_binding(session_public_id, profile_id)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;

    async fn storage() -> (SqliteStorage, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("automation.db");
        let storage = SqliteStorage::connect(path).await.expect("connect");
        (storage, dir)
    }

    fn workspace_identity(path: &Path) -> DotagentsAutomationIdentity {
        DotagentsAutomationIdentity::new(DotagentsLayer::Workspace, Some(path), None::<String>)
    }

    #[test]
    fn binding_key_is_stable_and_distinguishes_scopes() {
        let base = workspace_identity(Path::new("/ws/one"));
        assert_eq!(base.binding_key(), base.clone().binding_key());
        assert!(
            base.binding_key()
                .starts_with("dotagents:v1:automation:workspace:")
        );

        let other_workspace = workspace_identity(Path::new("/ws/two"));
        assert_ne!(base.binding_key(), other_workspace.binding_key());

        let global = DotagentsAutomationIdentity::new(
            DotagentsLayer::Global,
            None::<PathBuf>,
            None::<String>,
        );
        assert_ne!(base.binding_key(), global.binding_key());

        let profiled = DotagentsAutomationIdentity::new(
            DotagentsLayer::Workspace,
            Some("/ws/one"),
            Some("coding"),
        );
        assert_ne!(base.binding_key(), profiled.binding_key());
    }

    #[tokio::test]
    async fn ensure_creates_once_and_reuses_across_restarts() {
        let (storage, _dir) = storage().await;
        let repo = SqliteDotagentsAutomationRepository::new(storage.conn());
        let identity = workspace_identity(Path::new("/ws/one"));

        let first = repo
            .ensure_automation_session(&identity)
            .await
            .expect("first ensure");
        let second = repo
            .ensure_automation_session(&identity)
            .await
            .expect("second ensure");

        assert_eq!(first.session_public_id, second.session_public_id);

        // A fresh repository over the same database behaves like a restart.
        let repo_after_restart = SqliteDotagentsAutomationRepository::new(storage.conn());
        let third = repo_after_restart
            .ensure_automation_session(&identity)
            .await
            .expect("ensure after restart");
        assert_eq!(first.session_public_id, third.session_public_id);

        let session_count = storage
            .session_store()
            .list_sessions()
            .await
            .expect("list sessions")
            .len();
        assert_eq!(session_count, 1, "exactly one automation session exists");
    }

    #[tokio::test]
    async fn ensure_tags_session_kind_and_binds_profile() {
        let (storage, _dir) = storage().await;
        let repo = SqliteDotagentsAutomationRepository::new(storage.conn());
        let identity = DotagentsAutomationIdentity::new(
            DotagentsLayer::Workspace,
            Some("/ws/one"),
            Some("coding"),
        );

        let binding = repo
            .ensure_automation_session(&identity)
            .await
            .expect("ensure");
        assert_eq!(binding.default_profile_id.as_deref(), Some("coding"));

        let session = storage
            .session_store()
            .get_session(&binding.session_public_id)
            .await
            .expect("get session")
            .expect("session exists");
        assert_eq!(
            session.session_kind.as_deref(),
            Some(AUTOMATION_SESSION_KIND)
        );
    }

    #[tokio::test]
    async fn find_does_not_create() {
        let (storage, _dir) = storage().await;
        let repo = SqliteDotagentsAutomationRepository::new(storage.conn());
        let identity = workspace_identity(Path::new("/ws/one"));

        assert!(
            repo.find_automation_session(&identity)
                .await
                .expect("find")
                .is_none()
        );
        repo.ensure_automation_session(&identity)
            .await
            .expect("ensure");
        assert!(
            repo.find_automation_session(&identity)
                .await
                .expect("find")
                .is_some()
        );
    }
}
