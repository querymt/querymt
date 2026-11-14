//! Durable registry contract for managed workspaces.
//!
//! Persists [`WorkspaceRecord`]s (the workspace registry) and retained
//! [`ResultProvenance`]s. Ownership uniqueness (one writable session owner
//! per managed workspace) and checkout-path uniqueness (removed paths are
//! never reused) are enforced by the implementation and surfaced as explicit
//! `SessionError` variants.

use super::domain::{
    Baseline, ProvisioningPhase, ResultProvenance, WorkspaceId, WorkspaceLifecycle, WorkspaceRecord,
};
use crate::session::error::SessionResult;
use async_trait::async_trait;
use std::path::Path;

/// Durable registry of managed workspaces.
#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    /// Insert a new workspace record. Fails if the checkout path or
    /// workspace id already exists.
    async fn insert_workspace(&self, record: WorkspaceRecord) -> SessionResult<WorkspaceRecord>;

    /// Fetch a workspace by id.
    async fn get_workspace(&self, id: &WorkspaceId) -> SessionResult<Option<WorkspaceRecord>>;

    /// Fetch a workspace by its checkout path (canonical absolute path).
    async fn get_workspace_by_checkout(
        &self,
        checkout_path: &Path,
    ) -> SessionResult<Option<WorkspaceRecord>>;

    /// Fetch the workspace owned by `session_id`, if any. At most one
    /// workspace can be owned by a session (unique writable-owner index),
    /// so this is the durable session→workspace binding used by load and
    /// resume.
    async fn get_workspace_by_session(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<WorkspaceRecord>>;

    /// The workspace allocated for one delegation, when it ran isolated.
    async fn get_workspace_by_delegation(
        &self,
        delegation_id: &str,
    ) -> SessionResult<Option<WorkspaceRecord>> {
        Ok(self
            .list_workspaces()
            .await?
            .into_iter()
            .find(|r| r.ownership.delegation_id.as_deref() == Some(delegation_id)))
    }

    /// List all workspaces, newest first.
    async fn list_workspaces(&self) -> SessionResult<Vec<WorkspaceRecord>>;

    /// List workspaces sharing one repository identity (canonical common
    /// directory), newest first.
    async fn list_workspaces_by_repository(
        &self,
        common_dir: &Path,
    ) -> SessionResult<Vec<WorkspaceRecord>>;

    /// List the workspaces of one parallel delegation batch.
    async fn list_workspaces_by_batch(&self, batch_id: &str)
    -> SessionResult<Vec<WorkspaceRecord>>;

    /// Full-row update of mutable workspace fields. The record must exist.
    async fn update_workspace(&self, record: WorkspaceRecord) -> SessionResult<()>;

    /// Compare-and-swap lifecycle transition. Returns `false` when the
    /// workspace is not in `from`.
    async fn transition_lifecycle(
        &self,
        id: &WorkspaceId,
        from: WorkspaceLifecycle,
        to: WorkspaceLifecycle,
    ) -> SessionResult<bool>;

    /// Bind the session that owns this workspace. Fails with
    /// [`SessionError::WorkspaceAlreadyOwned`] when another session already
    /// owns the workspace (unique writable-owner rule).
    async fn attach_owner(
        &self,
        id: &WorkspaceId,
        owner_session_id: &str,
    ) -> SessionResult<WorkspaceRecord>;

    /// Clear the session owner binding.
    async fn detach_owner(&self, id: &WorkspaceId) -> SessionResult<WorkspaceRecord>;

    /// Atomically move ownership of a `ready` workspace from `from` to `to`.
    ///
    /// One conditional `UPDATE ... WHERE owner_session_id = from AND
    /// lifecycle = 'ready'` plus the previous owner's transferred marker, in a
    /// single transaction: there is never an ownerless window, and of two
    /// racing handoffs exactly one wins. Fails with
    /// [`SessionError::WorkspaceAlreadyOwned`] when `from` is not the current
    /// owner and [`SessionError::InvalidOperation`] when the workspace is not
    /// `ready`.
    async fn transfer_owner(
        &self,
        id: &WorkspaceId,
        from: &str,
        to: &str,
    ) -> SessionResult<WorkspaceRecord>;

    /// The session `session_id` handed its workspace to, if it was
    /// transferred. Marks the session as a read-only previous owner.
    async fn transferred_to(&self, session_id: &str) -> SessionResult<Option<String>>;

    /// The session that owns the workspace `session_id` handed off, following
    /// repeated handoffs (A -> B -> C) to the current owner. `None` when the
    /// session never handed anything off. Bounded, so a corrupt cycle cannot
    /// loop forever.
    async fn current_owner_after_handoff(&self, session_id: &str) -> SessionResult<Option<String>> {
        let mut current = match self.transferred_to(session_id).await? {
            Some(next) => next,
            None => return Ok(None),
        };
        for _ in 0..32 {
            match self.transferred_to(&current).await? {
                Some(next) => current = next,
                None => break,
            }
        }
        Ok(Some(current))
    }

    /// Persist the captured immutable baseline for a workspace.
    async fn bind_baseline(&self, id: &WorkspaceId, baseline: &Baseline) -> SessionResult<()>;

    /// Advance the durable provisioning phase of a `provisioning` workspace.
    ///
    /// Forward-only: an attempt to move to an equal or earlier phase is a
    /// no-op (`Ok(false)`), so idempotent replay can safely re-record steps.
    /// Unknown workspace ids also yield `Ok(false)`.
    async fn record_provisioning_phase(
        &self,
        id: &WorkspaceId,
        phase: ProvisioningPhase,
    ) -> SessionResult<bool>;

    /// Persist retained result provenance for a workspace.
    async fn record_result(
        &self,
        id: &WorkspaceId,
        provenance: &ResultProvenance,
    ) -> SessionResult<()>;

    /// Retained results for a workspace, newest first.
    async fn list_results(&self, id: &WorkspaceId) -> SessionResult<Vec<ResultProvenance>>;
}
