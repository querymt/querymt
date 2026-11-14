//! `StorageBackend` implementation for `SqliteStorage`.

use crate::session::sqlite_storage::SqliteStorage;
use async_trait::async_trait;
use querymt_agent_core::dotagents_tasks::DotagentsTaskStateRepository;
use querymt_agent_core::knowledge::KnowledgeStore;
use querymt_agent_core::session::backend::StorageBackend;
use querymt_agent_core::session::error::SessionResult;
use querymt_agent_core::session::projection::{EventJournal, ViewStore};
use querymt_agent_core::session::repository::ScheduleRepository;
use querymt_agent_core::session::store::SessionStore;
use std::path::PathBuf;
use std::sync::Arc;

/// SQLite implementation of StorageBackend.
///
/// Implements all storage traits on a single unified struct.
/// For single-process deployments with local persistence.
#[async_trait]
impl StorageBackend for SqliteStorage {
    fn session_store(&self) -> Arc<dyn SessionStore> {
        Arc::new(self.clone())
    }

    fn event_journal(&self) -> Arc<dyn EventJournal> {
        Arc::new(self.clone())
    }

    fn view_store(&self) -> Option<Arc<dyn ViewStore>> {
        Some(Arc::new(self.clone()))
    }

    fn schedule_repository(&self) -> Option<Arc<dyn ScheduleRepository>> {
        Some(Arc::new(
            crate::repo_schedule::SqliteScheduleRepository::new(self.conn()),
        ))
    }

    fn dotagents_task_state_repository(&self) -> Option<Arc<dyn DotagentsTaskStateRepository>> {
        Some(Arc::new(
            crate::repo_dotagents_tasks::SqliteDotagentsTaskStateRepository::new(self.conn()),
        ))
    }

    fn dotagents_automation_repository(
        &self,
    ) -> Option<Arc<dyn querymt_agent_core::dotagents_automation::DotagentsAutomationRepository>>
    {
        Some(Arc::new(
            crate::repo_dotagents_automation::SqliteDotagentsAutomationRepository::new(self.conn()),
        ))
    }

    fn knowledge_store(&self) -> Option<Arc<dyn KnowledgeStore>> {
        Some(Arc::new(
            crate::knowledge::sqlite::SqliteKnowledgeStore::new(self.conn()),
        ))
    }

    fn workspace_repository(
        &self,
    ) -> Option<Arc<dyn querymt_agent_core::worktree::repository::WorkspaceRepository>> {
        Some(Arc::new(
            crate::repo_workspace::SqliteWorkspaceRepository::new(self.conn()),
        ))
    }

    fn run_lease_root(&self) -> Option<PathBuf> {
        SqliteStorage::run_lease_root(self)
    }
}

impl SqliteStorage {
    /// Helper: connect to SQLite at the given path.
    ///
    /// Creates the database file if needed and applies pending migrations.
    pub async fn connect_backend(path: PathBuf) -> SessionResult<Self> {
        SqliteStorage::connect(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_backend_exposes_run_lease_root_beside_database() {
        let dir = tempfile::TempDir::new().unwrap();
        let storage = SqliteStorage::connect(dir.path().join("sessions.db"))
            .await
            .unwrap();
        assert_eq!(
            StorageBackend::run_lease_root(&storage),
            Some(dir.path().join("sessions.run-leases"))
        );

        let memory = SqliteStorage::connect(":memory:".into()).await.unwrap();
        assert_eq!(StorageBackend::run_lease_root(&memory), None);

        let explicit = memory.with_run_lease_root(dir.path().join("leases"));
        assert_eq!(
            StorageBackend::run_lease_root(&explicit),
            Some(dir.path().join("leases"))
        );
    }
}
