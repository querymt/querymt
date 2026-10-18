//! Live status, missing-workspace reconciliation, and conservative removal
//! for managed worktree workspaces.
//!
//! Complements [`crate::worktree::allocate`] (creation/recovery) with the
//! operations the management surface needs on *existing* workspaces:
//!
//! - [`WorkspaceManager::status`]: pure observation of live Git state
//!   (registration, working-tree cleanliness, branch, commits beyond the
//!   baseline). Dirty/clean is never persisted as lifecycle state.
//! - [`WorkspaceManager::reconcile`]: marks a workspace `Missing` when its
//!   checkout directory vanished; never resurrects or falls back.
//! - [`WorkspaceManager::remove`]: explicit, conservative removal. Refuses
//!   active owners, uncommitted/untracked/ignored content, and commits
//!   lacking a retained recovery reference; never deletes directories Git
//!   does not know as the managed checkout. Successful removal reports
//!   exactly which refs (if any) were retained.
//!
//! All Git administration runs under the cross-process repository lock, and
//! all removal decisions are re-validated from durable state inside that
//! lock, so concurrent processes cannot interleave or double-remove.

use crate::worktree::allocate::worktree_registered;
use crate::worktree::domain::{
    ManagedBranch, WorkspaceId, WorkspaceLifecycle, WorkspaceRecord, baseline_ref_for,
};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::repo_ops::{RepositoryOps, validate_ref_name};
use crate::worktree::repo_workspace::WorkspaceRepository;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Live, observed state of a managed workspace.
///
/// Ownership reflects the supplied [`WorkspaceRecord`]; enforcement always
/// re-reads durable state (see [`WorkspaceManager::remove`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceStatus {
    /// Durable lifecycle state of the record.
    pub lifecycle: WorkspaceLifecycle,
    /// Whether the checkout directory exists on disk.
    pub checkout_exists: bool,
    /// Whether Git registers the checkout as a linked worktree on the
    /// managed branch (only meaningful when the directory exists).
    pub registered: bool,
    /// Owning session, when the record has one (active workspace).
    pub active_owner: Option<String>,
    /// Branch checked out in the worktree (`None` when detached or absent).
    pub branch: Option<String>,
    /// HEAD commit of the checkout, when present.
    pub head_commit: Option<String>,
    /// Tracked files with uncommitted modifications (porcelain lines).
    pub uncommitted: Vec<String>,
    /// Untracked, non-ignored files.
    pub untracked: Vec<String>,
    /// Ignored files (excluded from baselines, still block removal).
    pub ignored: Vec<String>,
    /// Commits on the managed branch beyond the recorded baseline.
    pub unique_commit_count: u64,
}

impl WorkspaceStatus {
    /// Whether the checkout has uncommitted or untracked (non-ignored) work.
    pub fn is_dirty(&self) -> bool {
        !self.uncommitted.is_empty() || !self.untracked.is_empty()
    }

    /// Whether nothing in the checkout would be lost by removal.
    pub fn is_clean_for_removal(&self) -> bool {
        self.uncommitted.is_empty() && self.untracked.is_empty() && self.ignored.is_empty()
    }
}

/// Outcome of a successful [`WorkspaceManager::remove`].
///
/// Nothing in a removal report implies integration: retained refs are
/// reported so callers can disclose exactly what remains for recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalReport {
    pub workspace_id: WorkspaceId,
    /// Whether the checkout directory was removed on disk (`false` when it
    /// was already absent and only Git metadata was pruned).
    pub checkout_removed: bool,
    /// The managed branch, when it was deleted (only branches with no
    /// commits beyond the baseline are deleted).
    pub removed_branch: Option<ManagedBranch>,
    /// Refs intentionally retained for recovery (managed branch and/or
    /// recorded result refs).
    pub retained_refs: Vec<String>,
}

impl fmt::Display for RemovalReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "workspace {} removed", self.workspace_id)?;
        if !self.checkout_removed {
            write!(f, " (checkout was already absent)")?;
        }
        if let Some(branch) = &self.removed_branch {
            write!(f, "; deleted branch {branch}")?;
        }
        if !self.retained_refs.is_empty() {
            write!(f, "; retained refs: {}", self.retained_refs.join(", "))?;
        }
        Ok(())
    }
}

/// Live-status, reconciliation, and removal operations for managed
/// workspaces recorded in a [`WorkspaceRepository`].
pub struct WorkspaceManager {
    git: Arc<dyn RepositoryOps>,
    registry: Arc<dyn WorkspaceRepository>,
    storage_root: PathBuf,
}

impl WorkspaceManager {
    pub fn new(
        git: Arc<dyn RepositoryOps>,
        registry: Arc<dyn WorkspaceRepository>,
        storage_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            git,
            registry,
            storage_root: storage_root.into(),
        }
    }

    /// Observe live state of a workspace. Pure: never mutates Git or the
    /// registry. Missing checkouts are reported (`checkout_exists == false`)
    /// without changing lifecycle; persist that with [`Self::reconcile`].
    pub async fn status(&self, record: &WorkspaceRecord) -> WorktreeResult<WorkspaceStatus> {
        self.observe(record).await
    }

    /// Reconcile a workspace whose checkout directory disappeared: the
    /// durable lifecycle moves `Ready` -> `Missing` with a diagnostic.
    /// Idempotent for already-`Missing` records and a no-op when the
    /// checkout is present. Provisioning records must go through
    /// [`crate::worktree::allocate::WorkspaceAllocator::recover`] instead.
    pub async fn reconcile(&self, record: &WorkspaceRecord) -> WorktreeResult<WorkspaceRecord> {
        if record.lifecycle == WorkspaceLifecycle::Provisioning {
            return Err(WorktreeError::InvalidOperation(format!(
                "workspace {} is provisioning; use workspace allocation recovery for interrupted provisioning",
                record.id
            )));
        }
        if record.lifecycle != WorkspaceLifecycle::Ready || record.checkout_path.is_dir() {
            return Ok(record.clone());
        }

        let diagnostic = format!(
            "checkout directory {} is missing; the workspace must be re-created or manually recovered",
            record.checkout_path.display()
        );
        let moved = self
            .registry
            .transition_lifecycle(
                &record.id,
                WorkspaceLifecycle::Ready,
                WorkspaceLifecycle::Missing,
            )
            .await?;
        if !moved {
            return Err(WorktreeError::InvalidOperation(format!(
                "workspace {} left ready state during missing-workspace reconciliation",
                record.id
            )));
        }
        let mut updated = self
            .registry
            .get_workspace(&record.id)
            .await?
            .ok_or_else(|| {
                WorktreeError::InvalidOperation(format!(
                    "workspace {} disappeared during reconciliation",
                    record.id
                ))
            })?;
        updated.last_error = Some(diagnostic);
        self.registry.update_workspace(updated.clone()).await?;
        Ok(updated)
    }

    /// Explicitly and conservatively remove a managed workspace.
    ///
    /// Refuses (without deleting anything):
    /// - records still provisioning or already removed;
    /// - workspaces with an active session owner;
    /// - checkouts containing uncommitted, untracked, or ignored files;
    /// - directories Git does not know as the managed checkout (externally
    ///   owned or unknown pre-existing content);
    /// - commits beyond the baseline with no retained recovery reference.
    ///
    /// On success the checkout is removed through Git administration, the
    /// managed branch is deleted only when it adds nothing beyond the
    /// baseline, and the report discloses every retained ref.
    /// Retire execution without dropping ownership or conversation history.
    pub async fn archive(&self, record: &WorkspaceRecord) -> WorktreeResult<WorkspaceRecord> {
        let _activity = crate::worktree::gate::CheckoutActivityLease::acquire(
            &self.storage_root,
            &record.checkout_path,
            true,
        )?;
        crate::worktree::lock::with_admin_lock(
            &self.storage_root,
            &record.repository.common_dir,
            async {
                let current = self
                    .registry
                    .get_workspace(&record.id)
                    .await?
                    .ok_or_else(|| {
                        WorktreeError::InvalidOperation("workspace disappeared".into())
                    })?;
                if current.lifecycle == WorkspaceLifecycle::Archived {
                    return Ok(current);
                }
                if !matches!(
                    current.lifecycle,
                    WorkspaceLifecycle::Ready
                        | WorkspaceLifecycle::Missing
                        | WorkspaceLifecycle::Failed
                ) {
                    return Err(WorktreeError::InvalidOperation(
                        "only ready, missing or failed workspaces can be archived".into(),
                    ));
                }
                if !self
                    .registry
                    .transition_lifecycle(
                        &current.id,
                        current.lifecycle,
                        WorkspaceLifecycle::Archived,
                    )
                    .await?
                {
                    return Err(WorktreeError::InvalidOperation(
                        "workspace changed during archival".into(),
                    ));
                }
                self.registry
                    .get_workspace(&record.id)
                    .await?
                    .ok_or_else(|| WorktreeError::InvalidOperation("workspace disappeared".into()))
            },
        )
        .await
    }

    pub async fn remove(&self, record: &WorkspaceRecord) -> WorktreeResult<RemovalReport> {
        Self::ensure_removable(record)?;

        // Authoritative durable state: the caller's copy may be stale.
        let current = self
            .registry
            .get_workspace(&record.id)
            .await?
            .ok_or_else(|| {
                WorktreeError::InvalidOperation(format!(
                    "workspace {} is no longer registered",
                    record.id
                ))
            })?;
        Self::ensure_removable(&current)?;

        if let Some(owner) = &current.ownership.owner_session_id
            && !matches!(
                current.lifecycle,
                WorkspaceLifecycle::Archived | WorkspaceLifecycle::Removing
            )
        {
            return Err(WorktreeError::InvalidOperation(format!(
                "workspace {} is active (owned by session {owner}); detach the owner session before removal",
                record.id
            )));
        }

        let _activity = crate::worktree::gate::CheckoutActivityLease::acquire(
            &self.storage_root,
            &current.checkout_path,
            true,
        )?;
        let common_dir = current.repository.common_dir.clone();
        let report =
            crate::worktree::lock::with_admin_lock(&self.storage_root, &common_dir, async {
                let current = self
                    .registry
                    .get_workspace(&current.id)
                    .await?
                    .ok_or_else(|| {
                        WorktreeError::InvalidOperation(
                            "workspace disappeared during removal".into(),
                        )
                    })?;
                Self::ensure_removable(&current)?;
                if current.ownership.owner_session_id.is_some()
                    && !matches!(
                        current.lifecycle,
                        WorkspaceLifecycle::Archived | WorkspaceLifecycle::Removing
                    )
                {
                    return Err(WorktreeError::InvalidOperation(
                        "workspace acquired an owner during removal".into(),
                    ));
                }
                self.remove_locked(&current).await
            })
            .await;

        match report {
            Ok(report) => Ok(report),
            Err(err) => {
                self.record_removal_failure(&record.id, &err).await;
                Err(err)
            }
        }
    }

    fn ensure_removable(record: &WorkspaceRecord) -> WorktreeResult<()> {
        match record.lifecycle {
            WorkspaceLifecycle::Ready
            | WorkspaceLifecycle::Archived
            | WorkspaceLifecycle::Failed
            | WorkspaceLifecycle::Missing => Ok(()),
            WorkspaceLifecycle::Provisioning => Err(WorktreeError::InvalidOperation(format!(
                "workspace {} is still provisioning; run recovery before removal",
                record.id
            ))),
            WorkspaceLifecycle::Removing => Ok(()),
            WorkspaceLifecycle::Removed => Err(WorktreeError::InvalidOperation(format!(
                "workspace {} is already removed",
                record.id
            ))),
        }
    }

    /// Removal steps, run under the repository administration lock with the
    /// freshly loaded record.
    async fn remove_locked(&self, record: &WorkspaceRecord) -> WorktreeResult<RemovalReport> {
        let repo_root = main_repo_root(&record.repository.common_dir)?;
        let checkout = record.checkout_path.clone();
        let checkout_exists = checkout.is_dir();

        // 1. Registration safety: a directory that is not a linked worktree,
        //    or is not registered at the managed branch, is not ours to
        //    delete — externally owned content is always preserved.
        if checkout_exists {
            if !checkout.join(".git").is_file() {
                return Err(WorktreeError::InvalidOperation(format!(
                    "preserving {}: it exists but is not a Git linked worktree (externally owned content is never deleted)",
                    checkout.display()
                )));
            }
            if !worktree_registered(self.git.as_ref(), repo_root, &checkout, &record.branch).await?
            {
                return Err(WorktreeError::InvalidOperation(format!(
                    "preserving {}: it is not a registered checkout of managed branch {}",
                    checkout.display(),
                    record.branch
                )));
            }
        }

        // 2. Content safety: any uncommitted, untracked, or ignored file
        //    blocks removal.
        if checkout_exists {
            let live = self.observe(record).await?;
            if !live.is_clean_for_removal() {
                return Err(WorktreeError::InvalidOperation(format!(
                    "workspace {} has unpreserved working state: {} uncommitted, {} untracked, {} ignored file(s); commit or remove them explicitly before removal",
                    record.id,
                    live.uncommitted.len(),
                    live.untracked.len(),
                    live.ignored.len()
                )));
            }
        }

        // 3. Result safety: commits beyond the baseline must be contained in
        //    a retained recovery reference before the checkout may go away.
        let result_refs: Vec<String> = self
            .registry
            .list_results(&record.id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|result| result.result_ref)
            .filter(|r| validate_ref_name(r).is_ok())
            .collect();
        let baseline_commit = record.baseline.as_ref().map(|b| b.baseline_commit.clone());
        let branch_tip = self.branch_tip(repo_root, &record.branch).await?;

        let mut commits_preserved = false;
        let mut retained_refs = result_refs.clone();
        // Baseline refs stay reachable while records reference them; they
        // are retained (never deleted) and disclosed.
        if record.baseline.is_some() {
            retained_refs.push(baseline_ref_for(&record.id));
        }
        match (&baseline_commit, &branch_tip) {
            (Some(base), Some(tip)) if tip == base => {
                // Baseline-only checkout: the branch adds nothing.
            }
            (Some(base), Some(tip)) => {
                let commits = self.commits_between(repo_root, base, tip).await?;
                let mut preserved = false;
                for r in &result_refs {
                    if self.is_ancestor(repo_root, tip, r).await? {
                        preserved = true;
                        break;
                    }
                }
                if !preserved {
                    return Err(WorktreeError::InvalidOperation(format!(
                        "workspace {} has {} commit(s) beyond its baseline without a retained recovery reference: [{}]; retain the result or integrate the commits before removal",
                        record.id,
                        commits.len(),
                        preview_commits(&commits)
                    )));
                }
                commits_preserved = true;
                retained_refs.push(record.branch.as_str().to_string());
            }
            (None, Some(_)) => {
                return Err(WorktreeError::InvalidOperation(format!(
                    "workspace {} has a managed branch but no recorded baseline; refusing removal",
                    record.id
                )));
            }
            (_, None) => {
                // Branch already absent: nothing more to preserve.
            }
        }

        // Claim removal before any destructive Git operation. Attaching an
        // owner uses a ready-only CAS, so it cannot race this transition.
        if record.lifecycle != WorkspaceLifecycle::Removing
            && !self
                .registry
                .transition_lifecycle(&record.id, record.lifecycle, WorkspaceLifecycle::Removing)
                .await?
        {
            return Err(WorktreeError::InvalidOperation(
                "workspace acquired an owner or changed lifecycle during removal".into(),
            ));
        }

        // 4. Remove the checkout through Git administration, or prune the
        //    metadata of a checkout that already vanished.
        let mut checkout_removed = false;
        if checkout_exists {
            self.git.remove_worktree(repo_root, &checkout).await?;
            checkout_removed = true;
        }
        self.git.prune_worktrees(repo_root).await?;

        // 5. Branch disposition: delete an empty managed branch; retain one
        //    whose commits are preserved elsewhere.
        let mut removed_branch = None;
        if branch_tip.is_some() && !commits_preserved {
            self.git.delete_branch(repo_root, &record.branch).await?;
            removed_branch = Some(record.branch.clone());
        }

        // 6. Completing an interrupted removal is idempotent under the admin lock.
        if !self
            .registry
            .transition_lifecycle(
                &record.id,
                WorkspaceLifecycle::Removing,
                WorkspaceLifecycle::Removed,
            )
            .await?
        {
            return Err(WorktreeError::InvalidOperation(format!(
                "workspace {} left removing state during removal",
                record.id
            )));
        }

        Ok(RemovalReport {
            workspace_id: record.id.clone(),
            checkout_removed,
            removed_branch,
            retained_refs,
        })
    }

    /// Best-effort diagnostic recording for refused or failed removals.
    async fn record_removal_failure(&self, id: &WorkspaceId, error: &WorktreeError) {
        if let Ok(Some(mut record)) = self.registry.get_workspace(id).await
            && matches!(
                record.lifecycle,
                WorkspaceLifecycle::Ready
                    | WorkspaceLifecycle::Failed
                    | WorkspaceLifecycle::Missing
                    | WorkspaceLifecycle::Archived
            )
        {
            record.last_error = Some(format!("removal refused: {error}"));
            let _ = self.registry.update_workspace(record).await;
        }
    }

    /// Live observation used by both [`Self::status`] and removal.
    async fn observe(&self, record: &WorkspaceRecord) -> WorktreeResult<WorkspaceStatus> {
        let checkout = &record.checkout_path;
        let mut status = WorkspaceStatus {
            lifecycle: record.lifecycle,
            checkout_exists: checkout.is_dir(),
            registered: false,
            active_owner: record.ownership.owner_session_id.clone(),
            branch: None,
            head_commit: None,
            uncommitted: Vec::new(),
            untracked: Vec::new(),
            ignored: Vec::new(),
            unique_commit_count: 0,
        };
        if !status.checkout_exists {
            return Ok(status);
        }

        let repo_root = main_repo_root(&record.repository.common_dir)?;

        // Registration of the checkout at the managed branch.
        status.registered =
            worktree_registered(self.git.as_ref(), repo_root, checkout, &record.branch).await?;

        // Live working-tree state across the whole checkout.
        let live = self.git.checkout_status(checkout).await?;
        status.uncommitted = live.uncommitted;
        status.untracked = live.untracked;
        status.ignored = live.ignored;

        // HEAD and branch probes (lenient: failed records may lack them).
        status.head_commit = self.git.head_commit(checkout).await.unwrap_or(None);
        status.branch = self.git.current_branch(checkout).await.unwrap_or(None);

        // Commits on the managed branch beyond the recorded baseline.
        if let Some(baseline) = &record.baseline
            && let Some(tip) = self.branch_tip(repo_root, &record.branch).await?
            && tip != baseline.baseline_commit
        {
            status.unique_commit_count = u64::try_from(
                self.git
                    .commits_between_count(repo_root, &baseline.baseline_commit, &tip)
                    .await?,
            )
            .unwrap_or(0);
        }

        Ok(status)
    }

    /// Full object id at the branch tip, or `None` when the branch is absent.
    async fn branch_tip(
        &self,
        repo_root: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<Option<String>> {
        self.git.branch_tip(repo_root, branch).await
    }

    /// Commits reachable from `tip` but not `base` (newest first).
    async fn commits_between(
        &self,
        repo_root: &Path,
        base: &str,
        tip: &str,
    ) -> WorktreeResult<Vec<String>> {
        self.git.commits_between(repo_root, base, tip).await
    }

    /// Whether `ancestor` is contained in `descendant`.
    async fn is_ancestor(
        &self,
        repo_root: &Path,
        ancestor: &str,
        descendant: &str,
    ) -> WorktreeResult<bool> {
        self.git.is_ancestor(repo_root, ancestor, descendant).await
    }

    /// Access the registry (used by tests to cross-check durable state).
    #[cfg(test)]
    pub(crate) fn registry(&self) -> &Arc<dyn WorkspaceRepository> {
        &self.registry
    }
}

/// Main working-tree root for a canonical common directory
/// (`<project>/.git` -> `<project>`).
fn main_repo_root(common_dir: &Path) -> WorktreeResult<&Path> {
    common_dir.parent().ok_or_else(|| {
        WorktreeError::InvalidOperation(format!(
            "cannot determine main repository root from common dir {}",
            common_dir.display()
        ))
    })
}

/// Bounded commit list for removal-refusal diagnostics.
fn preview_commits(commits: &[String]) -> String {
    const MAX: usize = 5;
    if commits.len() <= MAX {
        return commits.join(", ");
    }
    format!(
        "{} (+{} more)",
        commits[..MAX].join(", "),
        commits.len() - MAX
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema;
    use crate::worktree::GitRunner as _;
    use crate::worktree::GixRepoOps;
    use crate::worktree::allocate::WorkspaceAllocator;
    use crate::worktree::allocate::checkout_path_for;
    use crate::worktree::domain::{
        Baseline, BaselineMode, ExecutionMode, ResultProvenance, ResultVerificationStatus,
        WorkspaceOwnership,
    };
    use crate::worktree::git::CliGit;
    use crate::worktree::identity::resolve_repository;
    use crate::worktree::repo_workspace::SqliteWorkspaceRepository;
    use crate::worktree::test_support::{commit_files, init_repo, materialize};
    use std::fs;
    use tempfile::TempDir;

    fn shared_db() -> (
        Arc<std::sync::Mutex<rusqlite::Connection>>,
        SqliteWorkspaceRepository,
    ) {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        schema::init_schema(&mut conn).unwrap();
        // A dormant session row so owner foreign keys can be satisfied.
        conn.execute(
            "INSERT INTO sessions (public_id, name, created_at, updated_at) \
             VALUES ('sess-owner', 'owner', '2026-01-15T00:00:00Z', '2026-01-15T00:00:00Z')",
            [],
        )
        .unwrap();
        let conn = Arc::new(std::sync::Mutex::new(conn));
        let repo = SqliteWorkspaceRepository::new(conn.clone());
        (conn, repo)
    }

    /// Provision a real managed workspace from a fresh repository with
    /// `files` committed, plus a manager sharing the same registry.
    async fn provision(
        files: &[(&str, &str)],
    ) -> (TempDir, TempDir, WorkspaceRecord, WorkspaceManager) {
        let repo = init_repo();
        commit_files(repo.path(), "init", files);
        materialize(repo.path(), files);
        let storage = TempDir::new().unwrap();
        let (_conn, store) = shared_db();
        let store = Arc::new(store);
        let alloc =
            WorkspaceAllocator::new(Arc::new(GixRepoOps::new()), store.clone(), storage.path());
        let resolved = resolve_repository(repo.path()).unwrap();
        let record = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap();
        let manager = WorkspaceManager::new(Arc::new(GixRepoOps::new()), store, storage.path());
        (repo, storage, record, manager)
    }

    /// Commit additional work inside the workspace checkout (gix, no CLI
    /// identity needed), materialize the files, and sync the worktree index
    /// (gix commits never write an index, so `git status` would otherwise
    /// report phantom staged deletions and untracked files).
    async fn commit_in_checkout(checkout: &Path, message: &str, files: &[(&str, &str)]) {
        commit_files(checkout, message, files);
        materialize(checkout, files);
        let args = ["add", "-A"];
        CliGit::new()
            .run(checkout, &args)
            .await
            .unwrap()
            .into_checked(&args)
            .unwrap();
    }

    async fn branch_exists(repo_root: &Path, branch: &ManagedBranch) -> bool {
        CliGit::new()
            .run(repo_root, &["rev-parse", "--verify", branch.as_str()])
            .await
            .unwrap()
            .success
    }

    #[tokio::test]
    async fn archive_requires_inactivity_and_retains_binding_through_removal() {
        let (_repo, storage, record, manager) = provision(&[("file.txt", "one")]).await;
        manager
            .registry()
            .attach_owner(&record.id, "sess-owner")
            .await
            .unwrap();
        let active = crate::worktree::gate::CheckoutActivityLease::acquire(
            storage.path(),
            &record.checkout_path,
            false,
        )
        .unwrap();
        assert!(manager.archive(&record).await.is_err());
        drop(active);
        let archived = manager.archive(&record).await.unwrap();
        assert_eq!(archived.lifecycle, WorkspaceLifecycle::Archived);
        assert_eq!(
            archived.ownership.owner_session_id.as_deref(),
            Some("sess-owner")
        );
        assert!(
            manager
                .registry()
                .attach_owner(&record.id, "sess-owner")
                .await
                .is_err()
        );
        manager.remove(&archived).await.unwrap();
        let retained = manager
            .registry()
            .get_workspace_by_session("sess-owner")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.lifecycle, WorkspaceLifecycle::Removed);
        assert!(!retained.checkout_path.exists());
    }

    #[tokio::test]
    async fn interrupted_removal_blocks_owners_and_can_be_recovered() {
        let (repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        assert!(
            manager
                .registry()
                .transition_lifecycle(
                    &record.id,
                    WorkspaceLifecycle::Ready,
                    WorkspaceLifecycle::Removing
                )
                .await
                .unwrap()
        );
        assert!(
            manager
                .registry()
                .attach_owner(&record.id, "sess-owner")
                .await
                .is_err()
        );
        CliGit::new()
            .run(
                repo.path(),
                &["worktree", "remove", record.checkout_path.to_str().unwrap()],
            )
            .await
            .unwrap()
            .into_checked(&["worktree", "remove"])
            .unwrap();
        // Recovery accepts even a stale Ready record, reloads under the lock,
        // and completes the durable Removing intent after checkout deletion.
        let report = manager.remove(&record).await.unwrap();
        assert!(!report.checkout_removed);
        assert!(!branch_exists(repo.path(), &record.branch).await);
        assert_eq!(
            manager
                .registry()
                .get_workspace(&record.id)
                .await
                .unwrap()
                .unwrap()
                .lifecycle,
            WorkspaceLifecycle::Removed
        );
    }

    // -- Status -------------------------------------------------------------

    #[tokio::test]
    async fn status_reports_clean_registered_checkout() {
        let (_repo, _storage, record, manager) =
            provision(&[(".gitignore", "ign*\n"), ("file.txt", "one")]).await;

        let status = manager.status(&record).await.unwrap();

        assert_eq!(status.lifecycle, WorkspaceLifecycle::Ready);
        assert!(status.checkout_exists);
        assert!(status.registered);
        assert_eq!(status.branch.as_deref(), Some(record.branch.as_str()));
        assert!(status.head_commit.is_some());
        assert!(status.uncommitted.is_empty());
        assert!(status.untracked.is_empty());
        assert!(status.ignored.is_empty());
        assert_eq!(status.unique_commit_count, 0);
        assert_eq!(status.active_owner, None);
        assert!(!status.is_dirty());
        assert!(status.is_clean_for_removal());
    }

    #[tokio::test]
    async fn status_reports_uncommitted_untracked_and_ignored_files() {
        let (_repo, _storage, record, manager) =
            provision(&[(".gitignore", "ign*\n"), ("file.txt", "one")]).await;
        fs::write(record.checkout_path.join("file.txt"), "edited").unwrap();
        fs::write(record.checkout_path.join("new.txt"), "untracked").unwrap();
        fs::write(record.checkout_path.join("ign.env"), "ignored").unwrap();

        let status = manager.status(&record).await.unwrap();

        assert!(status.is_dirty());
        assert!(!status.is_clean_for_removal());
        assert!(
            status.uncommitted.iter().any(|p| p.contains("file.txt")),
            "uncommitted: {:?}",
            status.uncommitted
        );
        assert!(status.untracked.iter().any(|p| p == "new.txt"));
        assert!(status.ignored.iter().any(|p| p.contains("ign.env")));
        assert_eq!(status.unique_commit_count, 0);
    }

    #[tokio::test]
    async fn status_counts_commits_beyond_the_baseline() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        assert_eq!(
            manager.status(&record).await.unwrap().unique_commit_count,
            0
        );

        commit_in_checkout(
            &record.checkout_path,
            "delegate work",
            &[("file.txt", "one"), ("out.txt", "result")],
        )
        .await;

        let status = manager.status(&record).await.unwrap();
        assert_eq!(status.unique_commit_count, 1);
        let baseline = record.baseline.as_ref().unwrap().baseline_commit.clone();
        assert_ne!(status.head_commit.as_deref(), Some(baseline.as_str()));
    }

    #[tokio::test]
    async fn status_reports_missing_checkout_without_mutating_lifecycle() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        fs::remove_dir_all(&record.checkout_path).unwrap();

        let status = manager.status(&record).await.unwrap();

        assert!(!status.checkout_exists);
        assert!(!status.registered);
        assert!(!status.is_dirty());
        // Status is pure observation; lifecycle changes via `reconcile`.
        let stored = manager
            .registry()
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Ready);
    }

    // -- Missing-workspace reconciliation -----------------------------------

    #[tokio::test]
    async fn reconcile_persists_missing_lifecycle_with_diagnostic() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        fs::remove_dir_all(&record.checkout_path).unwrap();

        let reconciled = manager.reconcile(&record).await.unwrap();

        assert_eq!(reconciled.lifecycle, WorkspaceLifecycle::Missing);
        let diagnostic = reconciled.last_error.as_deref().unwrap();
        assert!(diagnostic.contains("missing"), "diagnostic: {diagnostic}");
        let stored = manager
            .registry()
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Missing);

        // Idempotent for already-missing records.
        let again = manager.reconcile(&reconciled).await.unwrap();
        assert_eq!(again.lifecycle, WorkspaceLifecycle::Missing);
    }

    #[tokio::test]
    async fn reconcile_keeps_present_ready_checkout() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;

        let reconciled = manager.reconcile(&record).await.unwrap();

        assert_eq!(reconciled.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(reconciled.last_error, None);
    }

    // -- Conservative removal -----------------------------------------------

    #[tokio::test]
    async fn remove_refuses_active_owner() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        manager
            .registry()
            .attach_owner(&record.id, "sess-owner")
            .await
            .unwrap();
        let mut owned = record.clone();
        owned.ownership.owner_session_id = Some("sess-owner".into());

        let err = manager.remove(&owned).await.unwrap_err();

        assert!(err.to_string().contains("sess-owner"), "error: {err}");
        assert!(
            record.checkout_path.is_dir(),
            "checkout must not be touched"
        );
        assert_eq!(
            fs::read_to_string(record.checkout_path.join("file.txt")).unwrap(),
            "one"
        );
        let stored = manager
            .registry()
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Ready);
    }

    #[tokio::test]
    async fn remove_refuses_dirty_checkout() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        fs::write(record.checkout_path.join("file.txt"), "edited").unwrap();

        let err = manager.remove(&record).await.unwrap_err();

        assert!(err.to_string().contains("uncommitted"), "error: {err}");
        assert!(record.checkout_path.is_dir());
        assert_eq!(
            fs::read_to_string(record.checkout_path.join("file.txt")).unwrap(),
            "edited"
        );
        assert!(
            branch_exists(_repo.path(), &record.branch).await,
            "branch must be retained on refusal"
        );
    }

    #[tokio::test]
    async fn remove_refuses_untracked_files() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        fs::write(record.checkout_path.join("new.txt"), "untracked").unwrap();

        let err = manager.remove(&record).await.unwrap_err();

        assert!(err.to_string().contains("untracked"), "error: {err}");
        assert!(record.checkout_path.join("new.txt").exists());
        assert!(record.checkout_path.is_dir());
    }

    #[tokio::test]
    async fn remove_refuses_ignored_files() {
        let (_repo, _storage, record, manager) =
            provision(&[(".gitignore", "ign*\n"), ("file.txt", "one")]).await;
        fs::write(record.checkout_path.join("ign.env"), "ignored").unwrap();

        let err = manager.remove(&record).await.unwrap_err();

        assert!(err.to_string().contains("ignored"), "error: {err}");
        assert!(record.checkout_path.join("ign.env").exists());
        assert!(record.checkout_path.is_dir());
    }

    #[tokio::test]
    async fn remove_refuses_externally_owned_directory() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        // Replace the managed checkout with a plain directory.
        fs::remove_dir_all(&record.checkout_path).unwrap();
        fs::create_dir_all(&record.checkout_path).unwrap();
        fs::write(record.checkout_path.join("user-file.txt"), "precious").unwrap();

        let err = manager.remove(&record).await.unwrap_err();

        assert!(err.to_string().contains("externally owned"), "error: {err}");
        assert!(
            record.checkout_path.join("user-file.txt").exists(),
            "unknown content at the managed path must be preserved"
        );
    }

    #[tokio::test]
    async fn remove_refuses_unpreserved_commits() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        commit_in_checkout(
            &record.checkout_path,
            "delegate work",
            &[("file.txt", "one"), ("out.txt", "result")],
        )
        .await;

        let err = manager.remove(&record).await.unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("retained recovery reference"),
            "error: {message}"
        );
        assert!(record.checkout_path.is_dir());
        assert!(
            branch_exists(_repo.path(), &record.branch).await,
            "branch with unpreserved commits must be retained"
        );
        assert_eq!(
            fs::read_to_string(record.checkout_path.join("out.txt")).unwrap(),
            "result"
        );
    }

    #[tokio::test]
    async fn remove_allows_clean_checkout_and_reports_removal() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;

        let report = manager.remove(&record).await.unwrap();

        assert_eq!(report.workspace_id, record.id);
        assert!(report.checkout_removed);
        assert_eq!(report.removed_branch.as_ref(), Some(&record.branch));
        // The baseline ref is retained (never deleted) and disclosed.
        assert_eq!(
            report.retained_refs,
            vec![baseline_ref_for(&record.id)],
            "clean removal retains exactly the baseline ref"
        );
        assert!(!record.checkout_path.exists(), "checkout removed from disk");
        assert!(
            !branch_exists(_repo.path(), &record.branch).await,
            "baseline-only managed branch is deleted"
        );

        let stored = manager
            .registry()
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Removed);

        // Worktree metadata pruned, and a second removal is refused.
        let listing = CliGit::new()
            .run(_repo.path(), &["worktree", "list", "--porcelain"])
            .await
            .unwrap()
            .stdout_str();
        assert!(!listing.contains(&record.checkout_path.to_string_lossy().to_string()));
        let err = manager.remove(&record).await.unwrap_err();
        assert!(err.to_string().contains("removed"), "error: {err}");
    }

    #[tokio::test]
    async fn remove_keeps_branch_when_result_ref_preserves_commits() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        commit_in_checkout(
            &record.checkout_path,
            "delegate work",
            &[("file.txt", "one"), ("out.txt", "result")],
        )
        .await;
        let tip = CliGit::new()
            .run(_repo.path(), &["rev-parse", record.branch.as_str()])
            .await
            .unwrap()
            .stdout_str()
            .trim()
            .to_string();
        CliGit::new()
            .run(
                _repo.path(),
                &["update-ref", "refs/qmt/results/test", tip.as_str()],
            )
            .await
            .unwrap()
            .into_checked(&["update-ref"])
            .unwrap();
        manager
            .registry()
            .record_result(
                &record.id,
                &ResultProvenance {
                    baseline_commit: record.baseline.as_ref().unwrap().baseline_commit.clone(),
                    result_commit: Some(tip),
                    result_ref: Some("refs/qmt/results/test".into()),
                    verification: Some(ResultVerificationStatus::Passed),
                    captured_at: time::OffsetDateTime::now_utc(),
                },
            )
            .await
            .unwrap();

        let report = manager.remove(&record).await.unwrap();

        assert!(report.checkout_removed);
        assert_eq!(
            report.removed_branch, None,
            "branch with preserved commits is retained"
        );
        assert!(
            report
                .retained_refs
                .contains(&"refs/qmt/results/test".to_string())
        );
        assert!(
            report
                .retained_refs
                .contains(&record.branch.as_str().to_string())
        );
        assert!(report.retained_refs.contains(&baseline_ref_for(&record.id)));
        assert!(!record.checkout_path.exists());
        assert!(
            branch_exists(_repo.path(), &record.branch).await,
            "retained branch survives removal"
        );
        let stored = manager
            .registry()
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Removed);
    }

    #[tokio::test]
    async fn remove_handles_missing_checkout_with_prune_and_branch_deletion() {
        let (_repo, _storage, record, manager) = provision(&[("file.txt", "one")]).await;
        fs::remove_dir_all(&record.checkout_path).unwrap();
        let missing = manager.reconcile(&record).await.unwrap();
        assert_eq!(missing.lifecycle, WorkspaceLifecycle::Missing);

        let report = manager.remove(&missing).await.unwrap();

        assert!(!report.checkout_removed, "nothing on disk to remove");
        assert_eq!(report.removed_branch.as_ref(), Some(&record.branch));
        assert!(!record.checkout_path.exists());
        let stored = manager
            .registry()
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Removed);
        let listing = CliGit::new()
            .run(_repo.path(), &["worktree", "list", "--porcelain"])
            .await
            .unwrap()
            .stdout_str();
        assert!(
            !listing.contains(&record.checkout_path.to_string_lossy().to_string()),
            "vanished worktree metadata must be pruned: {listing}"
        );
    }

    #[tokio::test]
    async fn remove_refuses_provisioning_records() {
        let (_repo, _storage, mut record, manager) = provision(&[("file.txt", "one")]).await;
        record.lifecycle = WorkspaceLifecycle::Provisioning;
        manager
            .registry()
            .update_workspace(record.clone())
            .await
            .unwrap();

        let err = manager.remove(&record).await.unwrap_err();

        assert!(err.to_string().contains("provisioning"), "error: {err}");
        assert!(record.checkout_path.is_dir());
    }

    // -- Documented recovery runbook ------------------------------------------
    //
    // Verifies the runbook in docs/docs/agent/index.md ("Worktree Workspaces
    // — Lifecycle operations and recovery") against an interrupted
    // temp-repository fixture: crash during creation recovers to `ready`, an
    // externally deleted checkout reconciles to `missing`, an active owner
    // blocks removal, and successful removal discloses the deleted branch.
    #[tokio::test]
    async fn documented_lifecycle_and_recovery_runbook_holds() {
        let repo = init_repo();
        commit_files(repo.path(), "init", &[("file.txt", "one")]);
        materialize(repo.path(), &[("file.txt", "one")]);
        let storage = TempDir::new().unwrap();
        let (_conn, store) = shared_db();
        let store = Arc::new(store);
        let alloc =
            WorkspaceAllocator::new(Arc::new(GixRepoOps::new()), store.clone(), storage.path());
        let manager = WorkspaceManager::new(Arc::new(GixRepoOps::new()), store, storage.path());
        let resolved = resolve_repository(repo.path()).unwrap();

        // Step 1: crash during creation. The intent is persisted and the
        // branch is allocated, but the checkout never appears.
        let interrupted_id = WorkspaceId::generate();
        let interrupted_branch = ManagedBranch::for_workspace(&interrupted_id);
        let base = CliGit::new()
            .run(repo.path(), &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .stdout_str()
            .trim()
            .to_string();
        let intent = WorkspaceRecord {
            id: interrupted_id.clone(),
            repository: resolved.identity.clone(),
            checkout_path: checkout_path_for(
                storage.path(),
                &resolved.identity.common_dir,
                &interrupted_id,
            ),
            branch: interrupted_branch.clone(),
            relative_cwd: PathBuf::new(),
            execution: ExecutionMode::Worktree,
            baseline: Some(Baseline {
                mode: BaselineMode::Commit,
                source_head: base.clone(),
                baseline_commit: base.clone(),
            }),
            lifecycle: WorkspaceLifecycle::Provisioning,
            ownership: WorkspaceOwnership::default(),
            last_error: None,
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
        };
        manager.registry().insert_workspace(intent).await.unwrap();
        CliGit::new()
            .run(
                repo.path(),
                &["branch", interrupted_branch.as_str(), base.as_str()],
            )
            .await
            .unwrap()
            .into_checked(&["branch"])
            .unwrap();

        // Recovery replays the idempotent provisioning steps to `ready`.
        let recovered = alloc
            .recover_pending(repo.path(), &resolved.identity.common_dir)
            .await
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, interrupted_id);
        assert_eq!(recovered[0].lifecycle, WorkspaceLifecycle::Ready);
        assert!(recovered[0].checkout_path.is_dir());

        // Step 2: externally deleted checkout is observed, then reconciled.
        // A second, healthy allocation plays the deleted-checkout role.
        let healthy = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap();
        manager
            .registry()
            .attach_owner(&healthy.id, "sess-owner")
            .await
            .unwrap();
        fs::remove_dir_all(&healthy.checkout_path).unwrap();
        assert!(!manager.status(&healthy).await.unwrap().checkout_exists);
        let missing = manager.reconcile(&healthy).await.unwrap();
        assert_eq!(missing.lifecycle, WorkspaceLifecycle::Missing);

        // Step 3: missing checkouts cannot acquire new owners; the existing
        // durable owner still blocks removal even with a stale record.
        assert!(
            manager
                .registry()
                .attach_owner(&healthy.id, "sess-owner")
                .await
                .is_err()
        );
        assert!(manager.remove(&missing).await.is_err());
        manager.registry().detach_owner(&healthy.id).await.unwrap();

        // After detaching, the missing workspace is removed safely; the
        // report discloses the deleted branch and pruned metadata.
        let report = manager.remove(&missing).await.unwrap();
        assert!(!report.checkout_removed);
        assert_eq!(report.removed_branch.as_ref(), Some(&healthy.branch));

        // The recovered workspace is inspectable and cleanly removable.
        let status = manager.status(&recovered[0]).await.unwrap();
        assert!(status.is_clean_for_removal());
        let report = manager.remove(&recovered[0]).await.unwrap();
        assert!(report.checkout_removed);
        assert_eq!(report.removed_branch.as_ref(), Some(&recovered[0].branch));
        assert!(!recovered[0].checkout_path.exists());
    }
}
