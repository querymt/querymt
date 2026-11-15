//! The `StorageBackend` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use crate::dotagents_tasks::DotagentsTaskStateRepository;
use crate::knowledge::KnowledgeStore;
use crate::session::projection::{EventJournal, ViewStore};
use crate::session::repository::ScheduleRepository;
use crate::session::store::SessionStore;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;

/// Unified storage backend providing both command and query side stores.
///
/// Implementations provide:
/// - `session_store()`: For session/message persistence (command side)
/// - `event_journal()`: For durable event persistence
/// - `view_store()`: For generating views (query side, optional)
/// - `schedule_repository()`: For schedule persistence (optional)
/// - `knowledge_store()`: For knowledge persistence (optional)
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Session persistence (command side).
    fn session_store(&self) -> Arc<dyn SessionStore>;

    /// Durable event journal.
    fn event_journal(&self) -> Arc<dyn EventJournal>;

    /// Projection queries (query side).
    /// Returns None if backend doesn't support local projections (e.g., Kafka).
    fn view_store(&self) -> Option<Arc<dyn ViewStore>>;

    /// Schedule persistence for the scheduler actor.
    /// Returns None if backend doesn't support scheduling.
    fn schedule_repository(&self) -> Option<Arc<dyn ScheduleRepository>> {
        None
    }

    /// Persisted protocol task ownership and fingerprint-bound trust decisions.
    /// Returns None if the backend cannot reconcile protocol-owned tasks safely.
    fn dotagents_task_state_repository(&self) -> Option<Arc<dyn DotagentsTaskStateRepository>> {
        None
    }

    /// Protocol-owned automation sessions.
    ///
    /// Returns None if the backend cannot provision the durable session that
    /// protocol repeat tasks bind their schedules to.
    fn dotagents_automation_repository(
        &self,
    ) -> Option<Arc<dyn crate::dotagents_automation::DotagentsAutomationRepository>> {
        None
    }

    /// Knowledge store for the knowledge tools.
    /// Returns None if backend doesn't support knowledge storage.
    fn knowledge_store(&self) -> Option<Arc<dyn KnowledgeStore>> {
        None
    }

    /// Managed worktree workspace registry (isolated Git execution workspaces).
    /// Returns None if the backend cannot persist workspace records.
    fn workspace_repository(
        &self,
    ) -> Option<Arc<dyn crate::worktree::repository::WorkspaceRepository>> {
        None
    }

    /// Directory of per-session run leases guarding this backend's journal.
    /// Returns None when the backend has no local, machine-scoped location;
    /// run leasing and orphaned-run reconciliation are then disabled.
    fn run_lease_root(&self) -> Option<PathBuf> {
        None
    }
}
