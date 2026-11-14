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

pub use crate::core_staging::dotagents_automation::{
    AUTOMATION_SESSION_KIND, DotagentsAutomationIdentity, DotagentsAutomationRepository,
};
use crate::session::error::SessionResult;
pub use crate::sqlite_staging::repo_dotagents_automation::SqliteDotagentsAutomationRepository;

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
    use querymt_agent_core::dotagents::layer::DotagentsLayer;
    use std::path::{Path, PathBuf};

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
