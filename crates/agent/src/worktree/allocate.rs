//! Provisioning of managed worktree workspaces.
//!
//! Allocates a unique managed branch (`qmt/workspace/<workspace-id>`) and a
//! linked-worktree checkout under a durable storage root, persisting the
//! provisioning intent *before* touching Git so interrupted creations are
//! recoverable via [`WorkspaceAllocator::recover`].
//!
//! Invariants enforced here:
//! - workspace ids are unique, so concurrent creations always produce
//!   distinct checkouts and branches;
//! - the source repository's HEAD, index, and working files are never
//!   modified: `git branch` and `git worktree add` only add refs and create
//!   the new checkout;
//! - lifecycle is explicit: the record starts `Provisioning` and only
//!   reaches `Ready` after the checkout exists and the session-relative cwd
//!   is present; any failure marks the record `Failed` with diagnostics;
//! - a subdirectory session whose directory is absent from the selected
//!   baseline fails explicitly and never attaches to the checkout root.

use crate::worktree::domain::{
    Baseline, BaselineMode, ExecutionMode, ManagedBranch, WorkspaceId, WorkspaceLifecycle,
    WorkspaceOwnership, WorkspaceRecord, baseline_ref_for,
};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::gate::CheckoutMutationGate;
use crate::worktree::identity::{ResolvedRepository, ensure_relative_cwd_exists};
use crate::worktree::repo_ops::{RepositoryOps, validate_ref_name};
use crate::worktree::repo_workspace::WorkspaceRepository;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Parameters for provisioning one managed workspace.
///
/// `preset_baseline` implements shared batch baselines: the first member of
/// a parallel delegation batch captures once, and its recorded baseline is
/// passed as the preset for every sibling so all siblings start from
/// identical contents. Later, independently requested delegations omit the
/// preset and capture the then-current parent state.
#[derive(Clone)]
pub struct AllocationRequest<'a> {
    pub source: &'a ResolvedRepository,
    /// Session cwd relative to the checkout root (empty for root sessions).
    pub relative_cwd: &'a Path,
    /// Explicit base ref for [`BaselineMode::Commit`] (default source HEAD).
    pub base_ref: Option<&'a str>,
    pub baseline_mode: BaselineMode,
    /// Delegation batch this workspace belongs to, persisted on ownership.
    pub delegation_batch_id: Option<String>,
    /// Workspace this workspace was allocated for (delegation parent), if
    /// any. Persisted with the provisioning intent so the association
    /// survives crashes between allocation and session creation.
    pub parent_workspace_id: Option<WorkspaceId>,
    /// Session that requested this allocation (delegation parent), if any.
    pub parent_session_id: Option<String>,
    /// Delegation this workspace was allocated for, if any.
    pub delegation_id: Option<String>,
    /// Baseline to reuse instead of resolving/capturing (batch siblings).
    pub preset_baseline: Option<&'a Baseline>,
    /// Parent session's checkout mutation gate, held around
    /// current-working-tree capture so cooperating writes cannot interleave.
    pub mutation_gate: Option<&'a CheckoutMutationGate>,
}

/// Provisions managed worktree workspaces under a durable storage root.
pub struct WorkspaceAllocator {
    git: Arc<dyn RepositoryOps>,
    registry: Arc<dyn WorkspaceRepository>,
    storage_root: PathBuf,
}

impl WorkspaceAllocator {
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

    /// Provision a managed workspace derived from `source`.
    ///
    /// `relative_cwd` is the session cwd relative to the checkout root
    /// (empty for checkout-root sessions); it must exist in the created
    /// checkout. `base_ref` selects the base commit (default `HEAD`), and
    /// the baseline is that commit: uncommitted local edits are excluded.
    pub async fn allocate(
        &self,
        source: &ResolvedRepository,
        relative_cwd: &Path,
        base_ref: Option<&str>,
    ) -> WorktreeResult<WorkspaceRecord> {
        self.allocate_with_baseline(source, relative_cwd, base_ref, BaselineMode::Commit, None)
            .await
    }

    /// Provision a managed workspace with an explicit baseline mode.
    ///
    /// - [`BaselineMode::Commit`]: branch from the resolved commit
    ///   (`base_ref` or source HEAD); uncommitted local edits are excluded
    ///   and disclosed by the caller.
    /// - [`BaselineMode::CurrentWorkingTree`]: capture the source's current
    ///   working contents into an immutable private baseline commit (under
    ///   `mutation_gate` when provided) and branch from it. Combining this
    ///   with `base_ref` is invalid and rejected: current-working-tree
    ///   capture always reflects the checkout's current working state.
    ///
    /// Either way an immutable baseline ref (`refs/qmt/baselines/<id>`) is
    /// created so the baseline stays reachable across restarts and Git GC.
    pub async fn allocate_with_baseline(
        &self,
        source: &ResolvedRepository,
        relative_cwd: &Path,
        base_ref: Option<&str>,
        baseline_mode: BaselineMode,
        mutation_gate: Option<&CheckoutMutationGate>,
    ) -> WorktreeResult<WorkspaceRecord> {
        self.allocate_with(AllocationRequest {
            source,
            relative_cwd,
            base_ref,
            baseline_mode,
            delegation_batch_id: None,
            parent_workspace_id: None,
            parent_session_id: None,
            delegation_id: None,
            preset_baseline: None,
            mutation_gate,
        })
        .await
    }

    /// Provision a workspace from a full [`AllocationRequest`].
    ///
    /// A preset baseline takes precedence over `base_ref`/`baseline_mode`
    /// (no capture runs); combining a preset with an explicit `base_ref` is
    /// rejected as ambiguous.
    pub async fn allocate_with(
        &self,
        request: AllocationRequest<'_>,
    ) -> WorktreeResult<WorkspaceRecord> {
        let AllocationRequest {
            source,
            relative_cwd,
            base_ref,
            baseline_mode,
            delegation_batch_id,
            parent_workspace_id,
            parent_session_id,
            delegation_id,
            preset_baseline,
            mutation_gate,
        } = request;

        if preset_baseline.is_some() && base_ref.is_some() {
            return Err(WorktreeError::InvalidOperation(
                "a preset batch baseline cannot be combined with an explicit base ref".to_string(),
            ));
        }
        if preset_baseline.is_none()
            && baseline_mode == BaselineMode::CurrentWorkingTree
            && base_ref.is_some()
        {
            return Err(WorktreeError::InvalidOperation(
                "an explicit base ref cannot be combined with current-working-tree \
                 baseline capture; that baseline always reflects the checkout's \
                 current working state"
                    .to_string(),
            ));
        }

        let id = WorkspaceId::generate();
        let branch = ManagedBranch::for_workspace(&id);
        let checkout_path = checkout_path_for(&self.storage_root, &source.identity.common_dir, &id);

        // 1. Persist the provisioning intent before any Git mutation.
        let record = WorkspaceRecord {
            id: id.clone(),
            repository: source.identity.clone(),
            checkout_path: checkout_path.clone(),
            branch: branch.clone(),
            relative_cwd: relative_cwd.to_path_buf(),
            execution: ExecutionMode::Worktree,
            baseline: None,
            lifecycle: WorkspaceLifecycle::Provisioning,
            ownership: WorkspaceOwnership {
                delegation_batch_id: delegation_batch_id.clone(),
                parent_workspace_id: parent_workspace_id.clone(),
                parent_session_id: parent_session_id.clone(),
                delegation_id: delegation_id.clone(),
                ..Default::default()
            },
            last_error: None,
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
        };
        self.registry.insert_workspace(record.clone()).await?;

        // 2..n Provision under the cross-process administration lock; any
        // failure marks the record Failed.
        let provision = self.provision(
            &id,
            source,
            relative_cwd,
            base_ref,
            baseline_mode,
            preset_baseline,
            &branch,
            &checkout_path,
            mutation_gate,
        );
        match crate::worktree::lock::with_admin_lock(
            &self.storage_root,
            &source.identity.common_dir,
            provision,
        )
        .await
        {
            Ok(()) => {
                let moved = self
                    .registry
                    .transition_lifecycle(
                        &id,
                        WorkspaceLifecycle::Provisioning,
                        WorkspaceLifecycle::Ready,
                    )
                    .await?;
                if !moved {
                    return Err(WorktreeError::InvalidOperation(format!(
                        "workspace {id} left provisioning state unexpectedly"
                    )));
                }
                // The registry record is the single source of truth (it
                // carries the transition's updated_at).
                self.registry.get_workspace(&id).await?.ok_or_else(|| {
                    WorktreeError::InvalidOperation(format!(
                        "workspace {id} disappeared after provisioning"
                    ))
                })
            }
            Err(err) => {
                self.mark_failed(&id, &err).await;
                Err(err)
            }
        }
    }

    /// Provisioning steps, idempotent at each boundary so an interrupted
    /// attempt can be replayed by [`Self::recover`]:
    ///
    /// 1. resolve + record the baseline on the intent (branch boundary
    ///    input), plus its managed baseline ref,
    /// 2. create the managed branch unless it already exists at the base,
    /// 3. create the linked checkout unless the path is already a registered
    ///    worktree of the managed branch,
    /// 4. validate the session-relative cwd exists in the checkout.
    #[allow(clippy::too_many_arguments)]
    async fn provision(
        &self,
        id: &WorkspaceId,
        source: &ResolvedRepository,
        relative_cwd: &Path,
        base_ref: Option<&str>,
        baseline_mode: BaselineMode,
        preset_baseline: Option<&Baseline>,
        branch: &ManagedBranch,
        checkout_path: &Path,
        mutation_gate: Option<&CheckoutMutationGate>,
    ) -> WorktreeResult<()> {
        let repo_root = &source.checkout.root;

        // 1. Resolve + record the baseline on the intent so recovery knows
        //    the expected branch tip. A preset batch baseline is reused
        //    verbatim (siblings share one capture); Commit mode branches
        //    from the resolved commit; current-working-tree mode captures
        //    the checkout's actual working state first (under the session's
        //    mutation gate when provided) and branches from that private
        //    baseline.
        validate_ref_name(branch.as_str())?;
        let baseline = match preset_baseline {
            Some(preset) => preset.clone(),
            None => match baseline_mode {
                BaselineMode::Commit => {
                    let base_commit =
                        resolve_base_commit(self.git.as_ref(), repo_root, base_ref).await?;
                    Baseline {
                        mode: BaselineMode::Commit,
                        source_head: base_commit.clone(),
                        baseline_commit: base_commit,
                    }
                }
                BaselineMode::CurrentWorkingTree => {
                    let capture = crate::worktree::capture::capture_working_state_validated(
                        self.git.as_ref(),
                        &source.checkout.root,
                        Default::default(),
                    );
                    let captured = match mutation_gate {
                        Some(gate) => match gate.run(capture).await {
                            Ok(inner) => inner,
                            Err(err) => return Err(err),
                        },
                        None => capture.await,
                    }?;
                    Baseline {
                        mode: BaselineMode::CurrentWorkingTree,
                        source_head: captured.source_head,
                        baseline_commit: captured.commit,
                    }
                }
            },
        };
        self.registry.bind_baseline(id, &baseline).await?;

        // 2. Keep the baseline reachable across restarts/GC through a
        //    managed ref (retained and reported on removal, never deleted).
        let ref_name = baseline_ref_for(id);
        self.git
            .set_ref(repo_root, ref_name.as_str(), &baseline.baseline_commit)
            .await?;

        // 3. Allocate the managed branch at the baseline commit.
        self.ensure_branch(repo_root, branch, &baseline.baseline_commit)
            .await?;

        // 4. Create the linked worktree checking out the managed branch.
        self.ensure_checkout(repo_root, checkout_path, branch)
            .await?;

        // 5. The checkout root must exist and contain the session cwd.
        if !checkout_path.is_dir() {
            return Err(WorktreeError::InvalidOperation(format!(
                "worktree checkout missing after add: {}",
                checkout_path.display()
            )));
        }
        ensure_relative_cwd_exists(checkout_path, relative_cwd)
    }

    /// Create the managed branch at `expected_base` unless it already
    /// exists there. A branch pointing elsewhere is a hard conflict.
    async fn ensure_branch(
        &self,
        repo_root: &Path,
        branch: &ManagedBranch,
        expected_base: &str,
    ) -> WorktreeResult<()> {
        if let Some(tip) = self.git.branch_tip(repo_root, branch).await? {
            if tip == expected_base {
                return Ok(());
            }
            return Err(WorktreeError::InvalidOperation(format!(
                "managed branch {} exists at {} but the recorded base is {}",
                branch, tip, expected_base
            )));
        }
        self.git
            .create_branch(repo_root, branch, expected_base)
            .await
    }

    /// Create the linked checkout unless the path is already a registered
    /// worktree of `branch`. A pre-existing directory that Git does not
    /// know as this worktree is never touched — it is preserved and the
    /// operation fails loudly.
    async fn ensure_checkout(
        &self,
        repo_root: &Path,
        checkout_path: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<()> {
        if checkout_path.exists() {
            if worktree_registered(self.git.as_ref(), repo_root, checkout_path, branch).await? {
                return Ok(());
            }
            return Err(WorktreeError::InvalidOperation(format!(
                "preserving pre-existing unknown directory at {}: it is not a registered checkout of {}",
                checkout_path.display(),
                branch
            )));
        }

        self.git
            .add_worktree(repo_root, checkout_path, branch)
            .await
    }

    /// Idempotently finish an interrupted provisioning attempt.
    ///
    /// Replays the boundary steps ([`Self::provision`]) for a record still
    /// in `Provisioning`; safe to run after a crash at any boundary. Never
    /// touches pre-existing unknown directories at the checkout path.
    pub async fn recover(
        &self,
        record: &WorkspaceRecord,
        repo_root: &Path,
    ) -> WorktreeResult<WorkspaceRecord> {
        if record.lifecycle != WorkspaceLifecycle::Provisioning {
            return Err(WorktreeError::InvalidOperation(format!(
                "workspace {} is {:?}, only provisioning workspaces can be recovered",
                record.id, record.lifecycle
            )));
        }
        let base_commit = record
            .baseline
            .as_ref()
            .map(|b| b.baseline_commit.clone())
            .ok_or_else(|| {
                WorktreeError::InvalidOperation(format!(
                    "provisioning intent for {} has no recorded base commit",
                    record.id
                ))
            })?;

        let common_dir = record.repository.common_dir.clone();
        let finish = async {
            self.ensure_branch(repo_root, &record.branch, &base_commit)
                .await?;
            self.ensure_checkout(repo_root, &record.checkout_path, &record.branch)
                .await?;
            ensure_relative_cwd_exists(&record.checkout_path, &record.relative_cwd)?;
            let moved = self
                .registry
                .transition_lifecycle(
                    &record.id,
                    WorkspaceLifecycle::Provisioning,
                    WorkspaceLifecycle::Ready,
                )
                .await?;
            if !moved {
                return Err(WorktreeError::InvalidOperation(format!(
                    "workspace {} left provisioning state during recovery",
                    record.id
                )));
            }
            Ok(())
        };
        crate::worktree::lock::with_admin_lock(&self.storage_root, &common_dir, finish).await?;

        self.registry
            .get_workspace(&record.id)
            .await?
            .ok_or_else(|| {
                WorktreeError::InvalidOperation(format!(
                    "workspace {} disappeared during recovery",
                    record.id
                ))
            })
    }

    /// Recover every `Provisioning` workspace of a repository. Per-record
    /// failures are skipped (they stay `Provisioning`/`Failed` for the
    /// management surface); successes are returned.
    pub async fn recover_pending(
        &self,
        repo_root: &Path,
        common_dir: &Path,
    ) -> WorktreeResult<Vec<WorkspaceRecord>> {
        let pending: Vec<WorkspaceRecord> = self
            .registry
            .list_workspaces_by_repository(common_dir)
            .await?
            .into_iter()
            .filter(|r| r.lifecycle == WorkspaceLifecycle::Provisioning)
            .collect();

        let mut recovered = Vec::new();
        for record in pending {
            if let Ok(done) = self.recover(&record, repo_root).await {
                recovered.push(done);
            }
        }
        Ok(recovered)
    }

    /// Best-effort failure recording; never masks the original error.
    async fn mark_failed(&self, id: &WorkspaceId, error: &WorktreeError) {
        let _ = self
            .registry
            .transition_lifecycle(
                id,
                WorkspaceLifecycle::Provisioning,
                WorkspaceLifecycle::Failed,
            )
            .await;
        if let Ok(Some(mut record)) = self.registry.get_workspace(id).await {
            record.last_error = Some(error.to_string());
            let _ = self.registry.update_workspace(record).await;
        }
    }

    /// Access the typed repository operations (used by tests).
    #[cfg(test)]
    pub(crate) fn git(&self) -> &Arc<dyn RepositoryOps> {
        &self.git
    }
}

/// Whether `checkout_path` is a linked worktree of the shared repository
/// registered on `branch`.
pub(crate) async fn worktree_registered(
    ops: &dyn RepositoryOps,
    repo_root: &Path,
    checkout_path: &Path,
    branch: &ManagedBranch,
) -> WorktreeResult<bool> {
    let checkout_canonical = std::fs::canonicalize(checkout_path).ok();
    for registered in ops.list_worktrees(repo_root).await? {
        if registered.is_main || registered.branch.as_deref() != Some(branch.as_str()) {
            continue;
        }
        if let Ok(registered_canonical) = std::fs::canonicalize(&registered.path)
            && checkout_canonical
                .as_ref()
                .is_some_and(|c| *c == registered_canonical)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Durable checkout path: `<storage_root>/<repo-slug>/<workspace-id>`.
///
/// The repo slug is a stable digest of the canonical common directory so
/// all workspaces of one project group under one directory without relying
/// on (possibly colliding) directory names.
pub fn checkout_path_for(
    storage_root: &Path,
    common_dir: &Path,
    workspace_id: &WorkspaceId,
) -> PathBuf {
    storage_root
        .join(repo_slug(common_dir))
        .join(workspace_id.as_str())
}

/// FNV-1a 64-bit digest of the canonical common directory, hex-encoded.
pub(crate) fn repo_slug(common_dir: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in common_dir.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Resolve `base_ref` (or HEAD) to a full commit object id.
async fn resolve_base_commit(
    ops: &dyn RepositoryOps,
    repo_root: &Path,
    base_ref: Option<&str>,
) -> WorktreeResult<String> {
    let expr = base_ref.unwrap_or("HEAD");
    validate_rev_expression(expr)?;
    let rev = format!("{expr}^{{commit}}");
    match ops.resolve_commit(repo_root, &rev).await? {
        Some(commit) => Ok(commit),
        None => Err(WorktreeError::GitCommand {
            argv: rev,
            status: "rev-parse failed".to_string(),
            stderr: format!("{expr} does not resolve to a commit"),
        }),
    }
}

/// Validate a revision *expression* (may contain `~`, `^`, `@{…}` unlike a
/// ref name, but must never look like a git option or carry control
/// characters).
fn validate_rev_expression(expr: &str) -> WorktreeResult<()> {
    if expr.is_empty() {
        return Err(WorktreeError::GitCommand {
            argv: expr.to_string(),
            status: "validation rejected".to_string(),
            stderr: "empty revision expression".to_string(),
        });
    }
    if expr.starts_with('-') {
        return Err(WorktreeError::GitCommand {
            argv: expr.to_string(),
            status: "validation rejected".to_string(),
            stderr: "revision expression must not start with '-'".to_string(),
        });
    }
    if expr.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(WorktreeError::GitCommand {
            argv: expr.to_string(),
            status: "validation rejected".to_string(),
            stderr: "revision expression must not contain control characters".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema;
    use crate::worktree::GitRunner as _;
    use crate::worktree::GixRepoOps;
    use crate::worktree::git::CliGit;
    use crate::worktree::repo_workspace::SqliteWorkspaceRepository;
    use crate::worktree::test_support::{init_repo_with_subdir, materialize};
    use rusqlite::Connection;
    use std::sync::Mutex;
    use std::time::SystemTime;
    use tempfile::TempDir;

    fn allocator(storage: &Path) -> WorkspaceAllocator {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        schema::init_schema(&mut conn).unwrap();
        WorkspaceAllocator::new(
            Arc::new(GixRepoOps::new()),
            Arc::new(SqliteWorkspaceRepository::new(Arc::new(Mutex::new(conn)))),
            storage.to_path_buf(),
        )
    }

    fn resolve(repo_root: &Path) -> ResolvedRepository {
        crate::worktree::identity::resolve_repository(repo_root).unwrap()
    }

    /// Byte snapshot of a file, if present.
    fn snapshot_file(path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }

    /// (path, modified, len) snapshot of the working tree's regular files.
    fn snapshot_tree(root: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, SystemTime, u64)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.file_name().unwrap() == ".git" {
                    continue;
                }
                let meta = entry.metadata().unwrap();
                if meta.is_dir() {
                    walk(&path, out);
                } else {
                    out.push((path, meta.modified().unwrap(), meta.len()));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out.sort();
        out
    }

    use std::fs;

    #[tokio::test]
    async fn allocate_creates_ready_checkout_from_head() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap();

        assert_eq!(record.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(record.branch, ManagedBranch::for_workspace(&record.id));
        assert_eq!(record.session_cwd(), record.checkout_path);

        // Checkout materialized with the committed contents.
        assert!(record.checkout_path.is_dir());
        assert_eq!(
            fs::read_to_string(record.checkout_path.join("file.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            fs::read_to_string(record.checkout_path.join("sub/file.txt")).unwrap(),
            "sub"
        );

        // The managed branch points at the source HEAD.
        let out = CliGit::new()
            .run(repo.path(), &["rev-parse", record.branch.as_str()])
            .await
            .unwrap()
            .into_checked(&["rev-parse", record.branch.as_str()])
            .unwrap();
        assert_eq!(out.stdout_str().trim(), head.to_string());

        // Registry agrees.
        let stored = alloc
            .registry
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, record);
        assert_eq!(
            stored.repository.common_dir, resolved.identity.common_dir,
            "project identity groups workspaces by canonical common dir"
        );
    }

    #[tokio::test]
    async fn two_concurrent_creations_are_distinct_and_leave_source_untouched() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = std::sync::Arc::new(allocator(storage.path()));
        let resolved = resolve(repo.path());

        // Give the source a real index to observe (gix commits alone don't
        // write one), then snapshot HEAD, index bytes, and working files.
        CliGit::new()
            .run(repo.path(), &["reset", "--mixed", "HEAD"])
            .await
            .unwrap()
            .into_checked(&["reset", "--mixed", "HEAD"])
            .unwrap();

        let head_before = head.to_string();
        let index_before = snapshot_file(&repo.path().join(".git/index"));
        assert!(index_before.is_some(), "index must exist to be observed");
        let tree_before = snapshot_tree(repo.path());

        let a = alloc.clone();
        let b = alloc.clone();
        let resolved_a = resolved.clone();
        let (ra, rb) = tokio::join!(
            a.allocate(&resolved_a, Path::new(""), None),
            b.allocate(&resolved, Path::new(""), None),
        );
        let ra = ra.unwrap();
        let rb = rb.unwrap();

        // Distinct writable checkouts and branches.
        assert_ne!(ra.id, rb.id);
        assert_ne!(ra.checkout_path, rb.checkout_path);
        assert_ne!(ra.branch, rb.branch);
        assert_eq!(ra.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(rb.lifecycle, WorkspaceLifecycle::Ready);
        assert!(ra.checkout_path.join("file.txt").exists());
        assert!(rb.checkout_path.join("file.txt").exists());

        // Each checkout is independently writable.
        fs::write(ra.checkout_path.join("marker"), "a").unwrap();
        fs::write(rb.checkout_path.join("marker"), "b").unwrap();
        assert_eq!(
            fs::read_to_string(ra.checkout_path.join("marker")).unwrap(),
            "a"
        );
        assert_eq!(
            fs::read_to_string(rb.checkout_path.join("marker")).unwrap(),
            "b"
        );
        assert!(
            !repo.path().join("marker").exists(),
            "markers must not leak into the source checkout"
        );

        // Source HEAD, index, and working files unchanged.
        let head_after = CliGit::new()
            .run(repo.path(), &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .into_checked(&["rev-parse", "HEAD"])
            .unwrap();
        assert_eq!(head_after.stdout_str().trim(), head_before);
        assert_eq!(
            snapshot_file(&repo.path().join(".git/index")),
            index_before,
            "source index must not be modified by provisioning"
        );
        assert_eq!(
            snapshot_tree(repo.path()),
            tree_before,
            "source working files must not be modified by provisioning"
        );
    }

    #[tokio::test]
    async fn missing_base_ref_fails_and_marks_record_failed() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let err = alloc
            .allocate(&resolved, Path::new(""), Some("refs/heads/nope"))
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::GitCommand { .. }), "{err:?}");

        let all = alloc.registry.list_workspaces().await.unwrap();
        assert_eq!(all.len(), 1, "intent was persisted");
        assert_eq!(all[0].lifecycle, WorkspaceLifecycle::Failed);
        assert!(all[0].last_error.is_some());
    }

    #[tokio::test]
    async fn absent_relative_cwd_fails_explicitly_and_marks_record_failed() {
        let (repo, first, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        // Baseline predates `sub/`, so a `sub` session cannot attach.
        let err = alloc
            .allocate(&resolved, Path::new("sub"), Some(&first.to_string()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WorktreeError::RelativeCwdAbsent { ref relative, .. } if relative == &PathBuf::from("sub")),
            "unexpected error: {err:?}"
        );

        let all = alloc.registry.list_workspaces().await.unwrap();
        assert_eq!(all[0].lifecycle, WorkspaceLifecycle::Failed);
        assert!(all[0].last_error.is_some());
    }

    #[test]
    fn checkout_paths_are_namespaced_by_project_and_workspace() {
        let storage = Path::new("/data/worktrees");
        let common = Path::new("/repo/.git");
        let id = WorkspaceId::from_raw("ws-1");
        let path = checkout_path_for(storage, common, &id);
        assert_eq!(
            path,
            PathBuf::from(format!("/data/worktrees/{:016x}/ws-1", {
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                for byte in common.to_string_lossy().as_bytes() {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
                hash
            }))
        );
    }

    #[test]
    fn revision_expressions_reject_option_like_input() {
        assert!(validate_rev_expression("HEAD").is_ok());
        assert!(validate_rev_expression("main~1").is_ok());
        assert!(validate_rev_expression("").is_err());
        assert!(validate_rev_expression("-rf").is_err());
        assert!(validate_rev_expression("--upload-pack=x").is_err());
        assert!(validate_rev_expression("a\nb").is_err());
    }

    // -- idempotent recovery (fault injection at each boundary) -------------

    use crate::worktree::domain::{Baseline, BaselineMode};

    /// Persist a `Provisioning` intent as `allocate` would leave it after a
    /// crash at the given boundary. `base` is the recorded base commit.
    async fn insert_intent(
        alloc: &WorkspaceAllocator,
        storage: &Path,
        resolved: &ResolvedRepository,
        id: &str,
        base: &str,
    ) -> WorkspaceRecord {
        let wid = WorkspaceId::from_raw(id);
        let record = WorkspaceRecord {
            id: wid,
            repository: resolved.identity.clone(),
            checkout_path: checkout_path_for(
                storage,
                &resolved.identity.common_dir,
                &WorkspaceId::from_raw(id),
            ),
            branch: ManagedBranch::for_workspace(&WorkspaceId::from_raw(id)),
            relative_cwd: PathBuf::new(),
            execution: ExecutionMode::Worktree,
            baseline: Some(Baseline {
                mode: BaselineMode::Commit,
                source_head: base.to_string(),
                baseline_commit: base.to_string(),
            }),
            lifecycle: WorkspaceLifecycle::Provisioning,
            ownership: WorkspaceOwnership::default(),
            last_error: None,
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
        };
        alloc
            .registry
            .insert_workspace(record.clone())
            .await
            .unwrap();
        record
    }

    async fn branch_tip(
        _alloc: &WorkspaceAllocator,
        repo: &Path,
        branch: &ManagedBranch,
    ) -> String {
        CliGit::new()
            .run(repo, &["rev-parse", "--verify", branch.as_str()])
            .await
            .unwrap()
            .into_checked(&["rev-parse", "--verify", branch.as_str()])
            .unwrap()
            .stdout_str()
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn recover_from_intent_only_completes_provisioning() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-intent",
            &head.to_string(),
        )
        .await;

        let done = alloc.recover(&record, repo.path()).await.unwrap();
        assert_eq!(done.lifecycle, WorkspaceLifecycle::Ready);
        assert!(done.checkout_path.join("file.txt").exists());
        assert_eq!(
            branch_tip(&alloc, repo.path(), &done.branch).await,
            head.to_string()
        );
    }

    #[tokio::test]
    async fn recover_reuses_branch_created_before_interruption() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-branch",
            &head.to_string(),
        )
        .await;
        // Crash boundary: branch exists, checkout does not.
        CliGit::new()
            .run(
                repo.path(),
                &["branch", record.branch.as_str(), &head.to_string()],
            )
            .await
            .unwrap()
            .into_checked(&["branch", record.branch.as_str()])
            .unwrap();

        let done = alloc.recover(&record, repo.path()).await.unwrap();
        assert_eq!(done.lifecycle, WorkspaceLifecycle::Ready);
        // The pre-existing branch was reused, not recreated or moved.
        assert_eq!(
            branch_tip(&alloc, repo.path(), &done.branch).await,
            head.to_string()
        );
        assert!(done.checkout_path.join("file.txt").exists());
    }

    #[tokio::test]
    async fn recover_reuses_registered_checkout_created_before_interruption() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-checkout",
            &head.to_string(),
        )
        .await;
        // Crash boundary: branch and registered checkout exist, binding does not.
        CliGit::new()
            .run(
                repo.path(),
                &["branch", record.branch.as_str(), &head.to_string()],
            )
            .await
            .unwrap()
            .into_checked(&["branch", record.branch.as_str()])
            .unwrap();
        let path = record.checkout_path.to_string_lossy().into_owned();
        CliGit::new()
            .run(
                repo.path(),
                &["worktree", "add", &path, record.branch.as_str()],
            )
            .await
            .unwrap()
            .into_checked(&["worktree", "add"])
            .unwrap();

        let done = alloc.recover(&record, repo.path()).await.unwrap();
        assert_eq!(done.lifecycle, WorkspaceLifecycle::Ready);
        assert!(done.checkout_path.join("file.txt").exists());
    }

    #[tokio::test]
    async fn recover_preserves_pre_existing_unknown_directory() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-unknown",
            &head.to_string(),
        )
        .await;

        // Something unmanaged occupies the checkout path: recovery must
        // refuse to touch it, not delete it.
        std::fs::create_dir_all(&record.checkout_path).unwrap();
        std::fs::write(record.checkout_path.join("keep.txt"), b"precious").unwrap();

        let err = alloc.recover(&record, repo.path()).await.unwrap_err();
        assert!(
            matches!(err, WorktreeError::InvalidOperation(ref m) if m.contains("preserving")),
            "unexpected error: {err:?}"
        );
        assert!(record.checkout_path.is_dir(), "unknown directory preserved");
        assert!(
            record.checkout_path.join("keep.txt").exists(),
            "unknown directory contents preserved"
        );
        // Still pending for the management surface to resolve explicitly.
        assert_eq!(
            alloc
                .registry
                .get_workspace(&record.id)
                .await
                .unwrap()
                .unwrap()
                .lifecycle,
            WorkspaceLifecycle::Provisioning
        );
    }

    #[tokio::test]
    async fn recover_detects_branch_pointing_elsewhere() {
        let (repo, first, second) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-mismatch",
            &first.to_string(),
        )
        .await;
        // The branch exists but at a different commit than the recorded base.
        CliGit::new()
            .run(
                repo.path(),
                &["branch", record.branch.as_str(), &second.to_string()],
            )
            .await
            .unwrap()
            .into_checked(&["branch", record.branch.as_str()])
            .unwrap();

        let err = alloc.recover(&record, repo.path()).await.unwrap_err();
        assert!(matches!(err, WorktreeError::InvalidOperation(_)), "{err:?}");
        assert_eq!(
            branch_tip(&alloc, repo.path(), &record.branch).await,
            second.to_string(),
            "mismatched branch must not be moved"
        );
    }

    #[tokio::test]
    async fn recover_pending_touches_only_provisioning_records() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let pending = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-pending",
            &head.to_string(),
        )
        .await;
        let ready = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap();

        let recovered = alloc
            .recover_pending(repo.path(), &resolved.identity.common_dir)
            .await
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, pending.id);
        assert_eq!(recovered[0].lifecycle, WorkspaceLifecycle::Ready);

        // The already-ready workspace is untouched.
        let still = alloc
            .registry
            .get_workspace(&ready.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still, ready);
    }

    // -- Baseline modes and immutable baseline refs (section 3.3) -----------

    #[tokio::test]
    async fn current_working_tree_allocation_captures_dirty_source_untouched() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        // Dirty the source: tracked edit, tracked deletion, non-ignored
        // untracked file, and an ignored file that must stay excluded.
        std::fs::write(repo.path().join("file.txt"), "dirty").unwrap();
        std::fs::remove_file(repo.path().join("sub/file.txt")).unwrap();
        std::fs::write(repo.path().join("untracked.txt"), "new").unwrap();
        std::fs::write(repo.path().join(".gitignore"), "ign*\n").unwrap();
        std::fs::write(repo.path().join("ign.env"), "secret").unwrap();

        let head_before = head.to_string();
        let index_before = {
            let out = CliGit::new()
                .run(repo.path(), &["ls-files", "-s"])
                .await
                .unwrap();
            out.stdout_str()
        };
        let status_before = {
            let out = CliGit::new()
                .run(repo.path(), &["status", "--porcelain"])
                .await
                .unwrap();
            out.stdout_str()
        };

        let record = alloc
            .allocate_with_baseline(
                &resolved,
                Path::new(""),
                None,
                BaselineMode::CurrentWorkingTree,
                None,
            )
            .await
            .unwrap();

        // Baseline provenance recorded and branched from.
        let baseline = record.baseline.as_ref().unwrap();
        assert_eq!(baseline.mode, BaselineMode::CurrentWorkingTree);
        assert_eq!(baseline.source_head, head_before);
        assert_ne!(
            baseline.baseline_commit, head_before,
            "dirty source needs a private baseline commit"
        );
        let tip = branch_tip(&alloc, repo.path(), &record.branch).await;
        assert_eq!(tip, baseline.baseline_commit);

        // The checkout reflects the working contents (working edits, the
        // deletion, and the non-ignored untracked file — not the ignored
        // one, which is disclosed in the capture).
        let checkout = &record.checkout_path;
        assert_eq!(
            std::fs::read_to_string(checkout.join("file.txt")).unwrap(),
            "dirty"
        );
        assert!(!checkout.join("sub/file.txt").exists());
        assert_eq!(
            std::fs::read_to_string(checkout.join("untracked.txt")).unwrap(),
            "new"
        );
        assert!(!checkout.join("ign.env").exists());

        // Immutable baseline ref keeps the private commit reachable.
        let ref_name = baseline_ref_for(&record.id);
        let out = CliGit::new()
            .run(repo.path(), &["rev-parse", ref_name.as_str()])
            .await
            .unwrap();
        assert!(out.success, "baseline ref must exist");
        assert_eq!(out.stdout_str().trim(), baseline.baseline_commit);

        // The source is untouched: HEAD, index bytes, status, and files.
        let head_after = {
            let out = CliGit::new()
                .run(repo.path(), &["rev-parse", "HEAD"])
                .await
                .unwrap();
            out.stdout_str().trim().to_string()
        };
        assert_eq!(head_after, head_before);
        assert_eq!(
            CliGit::new()
                .run(repo.path(), &["ls-files", "-s"])
                .await
                .unwrap()
                .stdout_str(),
            index_before
        );
        assert_eq!(
            CliGit::new()
                .run(repo.path(), &["status", "--porcelain"])
                .await
                .unwrap()
                .stdout_str(),
            status_before
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "dirty"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("ign.env")).unwrap(),
            "secret"
        );
    }

    #[tokio::test]
    async fn commit_mode_allocation_creates_baseline_ref_at_base() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap();

        let ref_name = baseline_ref_for(&record.id);
        let out = CliGit::new()
            .run(repo.path(), &["rev-parse", ref_name.as_str()])
            .await
            .unwrap();
        assert!(
            out.success,
            "commit-mode allocations keep a baseline ref too"
        );
        assert_eq!(out.stdout_str().trim(), head.to_string());
    }

    #[tokio::test]
    async fn baseline_ref_preserves_reachability_across_gc() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        // Dirty source -> private baseline commit that nothing else refs.
        std::fs::write(repo.path().join("file.txt"), "dirty").unwrap();
        let record = alloc
            .allocate_with_baseline(
                &resolved,
                Path::new(""),
                None,
                BaselineMode::CurrentWorkingTree,
                None,
            )
            .await
            .unwrap();
        let baseline = record.baseline.as_ref().unwrap().baseline_commit.clone();

        // Aggressive GC: expire every reflog and prune all unreachable
        // objects, as another process might after a restart.
        for args in [
            vec!["reflog", "expire", "--expire=now", "--all"],
            vec!["gc", "--prune=now"],
        ] {
            let out = CliGit::new().run(repo.path(), &args).await.unwrap();
            assert!(out.success, "git {args:?} failed: {}", out.stderr_str());
        }

        // The baseline ref still resolves and the private commit object
        // still exists — reachability survived the GC.
        let ref_name = baseline_ref_for(&record.id);
        let out = CliGit::new()
            .run(repo.path(), &["rev-parse", ref_name.as_str()])
            .await
            .unwrap();
        assert!(out.success);
        assert_eq!(out.stdout_str().trim(), baseline);
        let probe = ["cat-file", "-e", baseline.as_str()];
        let out = CliGit::new().run(repo.path(), &probe).await.unwrap();
        assert!(out.success, "baseline commit must survive GC");
    }

    #[tokio::test]
    async fn explicit_ref_with_current_working_tree_capture_is_rejected() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let err = alloc
            .allocate_with_baseline(
                &resolved,
                Path::new(""),
                Some("HEAD"),
                BaselineMode::CurrentWorkingTree,
                None,
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("cannot be combined"),
            "error: {err}"
        );
    }

    // -- Shared batch baselines (section 3.5) --------------------------------

    #[tokio::test]
    async fn batch_siblings_share_baseline_and_later_requests_capture_new_state() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = Arc::new(allocator(storage.path()));
        let resolved = resolve(repo.path());

        // Parent working state A.
        std::fs::write(repo.path().join("file.txt"), "dirty").unwrap();

        // First sibling of the batch captures.
        let sibling_a = alloc
            .allocate_with(AllocationRequest {
                source: &resolved,
                relative_cwd: Path::new(""),
                base_ref: None,
                baseline_mode: BaselineMode::CurrentWorkingTree,
                delegation_batch_id: Some("batch-1".into()),
                parent_workspace_id: None,
                parent_session_id: None,
                delegation_id: None,
                preset_baseline: None,
                mutation_gate: None,
            })
            .await
            .unwrap();
        let baseline_a = sibling_a.baseline.clone().unwrap();

        // Parent moves on while the batch is in flight.
        std::fs::write(repo.path().join("file.txt"), "changed").unwrap();

        // Second sibling reuses the batch baseline: identical starting
        // contents even though the parent changed in between.
        let sibling_b = alloc
            .allocate_with(AllocationRequest {
                source: &resolved,
                relative_cwd: Path::new(""),
                base_ref: None,
                baseline_mode: BaselineMode::CurrentWorkingTree,
                delegation_batch_id: Some("batch-1".into()),
                parent_workspace_id: None,
                parent_session_id: None,
                delegation_id: None,
                preset_baseline: Some(&baseline_a),
                mutation_gate: None,
            })
            .await
            .unwrap();

        assert_eq!(sibling_b.baseline, Some(baseline_a.clone()));
        assert_ne!(sibling_a.checkout_path, sibling_b.checkout_path);
        assert_eq!(
            std::fs::read_to_string(sibling_b.checkout_path.join("file.txt")).unwrap(),
            "dirty",
            "batch sibling starts from the shared capture, not the newer parent state"
        );

        // Batch association persisted on both siblings.
        for sibling in [&sibling_a, &sibling_b] {
            let stored = alloc
                .registry
                .get_workspace(&sibling.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                stored.ownership.delegation_batch_id.as_deref(),
                Some("batch-1")
            );
        }

        // A later, independently requested delegate captures the parent's
        // subsequent edits.
        let later = alloc
            .allocate_with(AllocationRequest {
                source: &resolved,
                relative_cwd: Path::new(""),
                base_ref: None,
                baseline_mode: BaselineMode::CurrentWorkingTree,
                delegation_batch_id: None,
                parent_workspace_id: None,
                parent_session_id: None,
                delegation_id: None,
                preset_baseline: None,
                mutation_gate: None,
            })
            .await
            .unwrap();
        let baseline_later = later.baseline.clone().unwrap();
        assert_ne!(baseline_later.baseline_commit, baseline_a.baseline_commit);
        assert_eq!(
            std::fs::read_to_string(later.checkout_path.join("file.txt")).unwrap(),
            "changed",
            "later delegates see the parent's subsequent edits"
        );
        let stored = alloc
            .registry
            .get_workspace(&later.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.ownership.delegation_batch_id, None);
    }

    #[tokio::test]
    async fn preset_baseline_with_explicit_base_ref_is_rejected() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let preset = Baseline {
            mode: BaselineMode::Commit,
            source_head: "aaa".into(),
            baseline_commit: "aaa".into(),
        };
        let err = alloc
            .allocate_with(AllocationRequest {
                source: &resolved,
                relative_cwd: Path::new(""),
                base_ref: Some("HEAD"),
                baseline_mode: BaselineMode::Commit,
                delegation_batch_id: None,
                parent_workspace_id: None,
                parent_session_id: None,
                delegation_id: None,
                preset_baseline: Some(&preset),
                mutation_gate: None,
            })
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("cannot be combined"),
            "error: {err}"
        );
    }

    // Re-exported materialize helper keeps the import used in every cfg.
    #[allow(unused_imports)]
    use materialize as _materialize;
}
