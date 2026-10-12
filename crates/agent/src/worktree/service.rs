//! Session-facing workspace service.
//!
//! Bundles the allocator, lifecycle manager, and registry into the facade
//! the session materializer (and later the delegation orchestrator and
//! management surface) consume:
//!
//! - new sessions: allocate an isolated workspace when options request it
//!   (no-op for local execution), then bind the session as its single
//!   writable owner;
//! - load/resume: resolve the persisted session→workspace binding and fail
//!   closed for missing checkouts or conflicting cwd requests — execution
//!   never silently falls back to the original checkout.

use crate::session::error::SessionResult;
use crate::worktree::allocate::{AllocationRequest, WorkspaceAllocator};
use crate::worktree::capture::{CaptureRetryPolicy, capture_working_state_validated};
use crate::worktree::domain::{
    Baseline, BaselineMode, ResultProvenance, ResultVerificationStatus, WorkspaceId,
    WorkspaceLifecycle, WorkspaceRecord,
};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::gate::CheckoutMutationGate;
use crate::worktree::git::GitRunner;
use crate::worktree::identity::resolve_repository;
use crate::worktree::lifecycle::WorkspaceManager;
use crate::worktree::repo_workspace::WorkspaceRepository;
use crate::worktree::session_options::SessionCreationOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A resolved, durable binding between a session and its workspace.
#[derive(Debug, Clone)]
pub struct WorkspaceBinding {
    /// Durable workspace record (contains branch, baseline, ownership).
    pub record: WorkspaceRecord,
    /// The cwd the session must execute in.
    pub effective_cwd: PathBuf,
    /// Checkout-scoped mutation gate for the bound checkout.
    pub mutation_gate: Arc<CheckoutMutationGate>,
}

impl WorkspaceBinding {
    fn new(record: WorkspaceRecord, storage_root: &Path) -> WorktreeResult<Self> {
        let gate = Arc::new(CheckoutMutationGate::new(
            storage_root,
            &record.checkout_path,
        )?);
        Ok(Self {
            effective_cwd: record.session_cwd(),
            mutation_gate: gate,
            record,
        })
    }
}

/// Facade over managed workspace operations for the session layer.
pub struct WorkspaceService {
    git: Arc<dyn GitRunner>,
    allocator: WorkspaceAllocator,
    manager: WorkspaceManager,
    registry: Arc<dyn WorkspaceRepository>,
    storage_root: PathBuf,
}

impl WorkspaceService {
    pub fn new(
        git: Arc<dyn GitRunner>,
        registry: Arc<dyn WorkspaceRepository>,
        storage_root: impl Into<PathBuf>,
    ) -> Self {
        let storage_root = storage_root.into();
        Self {
            git: git.clone(),
            allocator: WorkspaceAllocator::new(git.clone(), registry.clone(), storage_root.clone()),
            manager: WorkspaceManager::new(git, registry.clone(), storage_root.clone()),
            registry,
            storage_root,
        }
    }

    pub fn registry(&self) -> &Arc<dyn WorkspaceRepository> {
        &self.registry
    }

    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    pub fn allocator(&self) -> &WorkspaceAllocator {
        &self.allocator
    }

    pub fn manager(&self) -> &WorkspaceManager {
        &self.manager
    }

    /// Execution mode of the session's bound workspace.
    ///
    /// Sessions with a persisted workspace binding execute in an isolated
    /// managed checkout (`Worktree`); unbound sessions keep legacy local
    /// execution (`Local`). This is the parent policy used to resolve
    /// inherited delegation isolation.
    pub async fn session_execution_mode(
        &self,
        session_id: &str,
    ) -> SessionResult<crate::worktree::ExecutionMode> {
        Ok(
            match self.registry.get_workspace_by_session(session_id).await? {
                Some(_) => crate::worktree::ExecutionMode::Worktree,
                None => crate::worktree::ExecutionMode::Local,
            },
        )
    }

    /// Allocate (but do not yet own) a workspace for a new session.
    ///
    /// Returns `Ok(None)` for local execution — no managed workspace is
    /// created and existing behavior is preserved. Isolation failures are
    /// errors: requested isolation never falls back to shared execution.
    pub async fn allocate_for_new_session(
        &self,
        source_cwd: &Path,
        options: &SessionCreationOptions,
    ) -> WorktreeResult<Option<WorkspaceBinding>> {
        options
            .validate()
            .map_err(WorktreeError::InvalidOperation)?;
        if !options.requests_isolation() {
            return Ok(None);
        }

        // Placement: this version supports isolated execution only on the
        // local filesystem host; remote paths fail before any dispatch.
        crate::worktree::placement::ensure_local_filesystem_execution(source_cwd)?;
        crate::worktree::placement::ensure_local_filesystem_execution(&self.storage_root)?;

        let resolved = resolve_repository(source_cwd)?;
        let relative =
            crate::worktree::identity::relative_cwd(&resolved.checkout.root, source_cwd)?;
        let record = self
            .allocator
            .allocate_with_baseline(
                &resolved,
                &relative,
                options.base_ref.as_deref(),
                options.baseline,
                None,
            )
            .await?;
        Ok(Some(WorkspaceBinding::new(record, &self.storage_root)?))
    }

    /// Bind a freshly created session as the workspace's single writable
    /// owner. Fails when another session already owns it (fail closed; a
    /// second writable attachment is rejected, not silently shared).
    pub async fn bind_owner(
        &self,
        workspace_id: &WorkspaceId,
        session_id: &str,
    ) -> WorktreeResult<WorkspaceRecord> {
        Ok(self.registry.attach_owner(workspace_id, session_id).await?)
    }

    /// Allocate a fresh working-state workspace for an isolated
    /// conversation fork through the same create path as fresh sessions.
    ///
    /// The fork's checkout lives under its own durable path — never the
    /// source session's checkout. The historical message fork point is
    /// recorded separately by the session store; this method only creates
    /// the new workspace and never touches the source's association.
    ///
    /// `source_cwd` is the source session's execution cwd (its bound
    /// checkout when the source itself is workspace-bound).
    /// `source_gate` serializes baseline capture against cooperating
    /// writes when the source is a live managed workspace.
    pub async fn allocate_for_fork(
        &self,
        source_cwd: &Path,
        options: &SessionCreationOptions,
        source_gate: Option<&CheckoutMutationGate>,
    ) -> WorktreeResult<WorkspaceBinding> {
        options
            .validate()
            .map_err(WorktreeError::InvalidOperation)?;
        if !options.requests_isolation() {
            return Err(WorktreeError::InvalidOperation(
                "fork workspace allocation requires worktree execution; local \
                 forks are pure message copies"
                    .to_string(),
            ));
        }

        // Placement: this version supports isolated execution only on the
        // local filesystem host; remote paths fail before any dispatch.
        crate::worktree::placement::ensure_local_filesystem_execution(source_cwd)?;
        crate::worktree::placement::ensure_local_filesystem_execution(&self.storage_root)?;

        let resolved = resolve_repository(source_cwd)?;
        let relative =
            crate::worktree::identity::relative_cwd(&resolved.checkout.root, source_cwd)?;
        let record = self
            .allocator
            .allocate_with_baseline(
                &resolved,
                &relative,
                options.base_ref.as_deref(),
                options.baseline,
                source_gate,
            )
            .await?;
        WorkspaceBinding::new(record, &self.storage_root)
    }

    /// Resolve the persisted workspace binding for load/resume.
    ///
    /// Returns `Ok(None)` when the session has no workspace (legacy local
    /// session). A bound session always resolves to its persisted checkout:
    ///
    /// - a conflicting requested cwd is rejected (the binding wins);
    /// - a missing checkout is reconciled to `Missing` and rejected —
    ///   no tools execute in the original checkout as a fallback;
    /// - non-`Ready` lifecycles are rejected with a diagnostic.
    pub async fn resolve_existing_session(
        &self,
        session_id: &str,
        requested_cwd: Option<&Path>,
    ) -> WorktreeResult<Option<WorkspaceBinding>> {
        let Some(record) = self.registry.get_workspace_by_session(session_id).await? else {
            return Ok(None);
        };

        if record.lifecycle != WorkspaceLifecycle::Ready {
            return Err(WorktreeError::InvalidOperation(format!(
                "workspace {} for session {session_id} is {} and cannot be attached",
                record.id, record.lifecycle
            )));
        }

        let effective_cwd = record.session_cwd();
        if let Some(requested) = requested_cwd {
            // Compare canonically on both sides: TempDir-style paths such as
            // macOS `/var/...` resolve to `/private/var/...`, and a caller
            // echoing back the bound cwd must never be rejected over a
            // symlink prefix.
            let requested =
                std::fs::canonicalize(requested).unwrap_or_else(|_| requested.to_path_buf());
            let bound =
                std::fs::canonicalize(&effective_cwd).unwrap_or_else(|_| effective_cwd.clone());
            if requested != bound {
                return Err(WorktreeError::SessionCwdConflict {
                    session_id: session_id.to_string(),
                    workspace_id: record.id.as_str().to_string(),
                    bound: effective_cwd,
                    requested,
                });
            }
        }

        // Fail closed when the checkout vanished: reconcile to `Missing`
        // (durable diagnostic) and reject attachment.
        if !record.checkout_path.is_dir() {
            self.manager.reconcile(&record).await?;
            return Err(WorktreeError::CheckoutMissing {
                workspace_id: record.id.as_str().to_string(),
                checkout: record.checkout_path.clone(),
            });
        }

        Ok(Some(WorkspaceBinding::new(record, &self.storage_root)?))
    }

    /// Capture the shared baseline for a delegation batch from the parent's
    /// current working state.
    ///
    /// Capture always reflects the parent's **current** tracked working-file
    /// contents, tracked deletions, and non-ignored untracked files (spec:
    /// private baseline includes parent edits) — a persisted creation-time
    /// baseline is never substituted for a dirty checkout. A clean parent
    /// re-derives the identical tree (capture reuses HEAD), so no redundant
    /// commits accumulate. Batch siblings share one capture via the preset
    /// baseline on the allocation request; later, independently requested
    /// delegations call this again and observe the parent's newer state.
    pub async fn capture_batch_baseline(
        &self,
        parent_cwd: &Path,
        baseline_mode: BaselineMode,
        mutation_gate: Option<&CheckoutMutationGate>,
    ) -> WorktreeResult<Baseline> {
        let capture = capture_working_state_validated(
            self.git.as_ref(),
            parent_cwd,
            CaptureRetryPolicy::default(),
        );
        let captured = match mutation_gate {
            Some(gate) => match gate.run(capture).await {
                Ok(inner) => inner,
                Err(err) => return Err(err),
            },
            None => capture.await,
        }?;
        Ok(Baseline {
            mode: baseline_mode,
            source_head: captured.source_head,
            baseline_commit: captured.commit,
        })
    }

    /// Allocate the child workspace for an isolated delegate — always before
    /// the delegate session exists (design decision 5).
    ///
    /// The batch baseline is captured (or selected, for an isolated parent)
    /// once per parallel batch so siblings start from identical contents;
    /// later, independently requested delegations call this again and see
    /// the parent's newer state. The association with the parent workspace,
    /// parent session, delegation, and batch is persisted with the
    /// provisioning intent, so it survives a crash between allocation and
    /// session creation. The parent's checkout, index, HEAD, and stash are
    /// never modified.
    pub async fn allocate_for_delegation(
        &self,
        parent_session_id: &str,
        parent_record: Option<&WorkspaceRecord>,
        parent_cwd: &Path,
        parent_gate: Option<&CheckoutMutationGate>,
        delegation_id: &str,
        delegation_batch_id: Option<String>,
    ) -> WorktreeResult<WorkspaceBinding> {
        // Placement: isolated delegation executes on the local filesystem
        // host in this version; anything else fails before any dispatch.
        crate::worktree::placement::ensure_local_filesystem_execution(parent_cwd)?;
        crate::worktree::placement::ensure_local_filesystem_execution(&self.storage_root)?;

        let resolved = resolve_repository(parent_cwd)?;
        let relative =
            crate::worktree::identity::relative_cwd(&resolved.checkout.root, parent_cwd)?;
        let fallback_gate = CheckoutMutationGate::new(&self.storage_root, &resolved.checkout.root)?;
        let parent_gate = Some(parent_gate.unwrap_or(&fallback_gate));

        // Delegates always start from the parent's current working state:
        // tracked working contents, tracked deletions, and non-ignored
        // untracked files, captured once per batch (spec: private baseline
        // includes parent edits).
        let batch_key = format!(
            "{}:{}",
            parent_session_id,
            delegation_batch_id.as_deref().unwrap_or(delegation_id)
        );
        let lock_path = self.storage_root.join("batch-locks").join(format!(
            "{}.lock",
            crate::hash::RapidHash::new(batch_key.as_bytes()).to_hex()
        ));
        crate::worktree::lock::with_lock_file(&lock_path, async {
            let existing = match delegation_batch_id.as_deref() {
                Some(batch) => self.registry.list_workspaces_by_batch(batch).await?,
                None => Vec::new(),
            };
            let baseline = match existing
                .iter()
                .find(|r| r.ownership.parent_session_id.as_deref() == Some(parent_session_id))
            {
                Some(record) => record.baseline.clone().ok_or_else(|| {
                    WorktreeError::InvalidOperation(
                        "batch workspace has no retained baseline".into(),
                    )
                })?,
                None => {
                    self.capture_batch_baseline(
                        parent_cwd,
                        BaselineMode::CurrentWorkingTree,
                        parent_gate,
                    )
                    .await?
                }
            };

            let record = self
                .allocator
                .allocate_with(AllocationRequest {
                    source: &resolved,
                    relative_cwd: &relative,
                    base_ref: None,
                    baseline_mode: BaselineMode::CurrentWorkingTree,
                    delegation_batch_id,
                    parent_workspace_id: parent_record.map(|r| r.id.clone()),
                    parent_session_id: Some(parent_session_id.to_string()),
                    delegation_id: Some(delegation_id.to_string()),
                    preset_baseline: Some(&baseline),
                    // The batch capture above already serialized against the
                    // parent's gate; provisioning only reads Git objects.
                    mutation_gate: None,
                })
                .await?;
            WorkspaceBinding::new(record, &self.storage_root)
        })
        .await
    }

    /// Capture an isolated delegate's uncommitted working state as an
    /// immutable, reviewable result (design decision 5).
    ///
    /// Reuses the repository capture primitive (a private temporary index),
    /// so the child's real staging state is untouched and no commit hooks
    /// run. The result commit's tree is the full working state at capture
    /// time; diffing it against the workspace baseline yields exactly the
    /// delegate's delta — inherited parent edits are part of the baseline
    /// and are never misattributed to the child. The result is pinned by a
    /// retained ref in the shared repository; the parent checkout is never
    /// touched.
    pub async fn capture_delegate_result(
        &self,
        record: &WorkspaceRecord,
        child_checkout: &Path,
        gate: Option<&CheckoutMutationGate>,
        verification: Option<ResultVerificationStatus>,
    ) -> WorktreeResult<ResultProvenance> {
        crate::worktree::placement::ensure_local_filesystem_execution(child_checkout)?;

        let capture = capture_working_state_validated(
            self.git.as_ref(),
            child_checkout,
            CaptureRetryPolicy::default(),
        );
        let captured = match gate {
            Some(gate) => gate.run(capture).await??,
            None => capture.await?,
        };

        // Unique create-only refs preserve results even after partial publication.
        let result_ref = format!("refs/qmt/results/{}/{}", record.id, uuid::Uuid::now_v7());
        self.git
            .run(
                child_checkout,
                &[
                    "update-ref",
                    result_ref.as_str(),
                    captured.commit.as_str(),
                    "",
                ],
            )
            .await?
            .into_checked(&["update-ref"])?;

        let provenance = ResultProvenance {
            baseline_commit: record
                .baseline
                .as_ref()
                .map(|b| b.baseline_commit.clone())
                .unwrap_or_else(|| captured.source_head.clone()),
            result_commit: Some(captured.commit.clone()),
            result_ref: Some(result_ref),
            verification,
            captured_at: time::OffsetDateTime::now_utc(),
        };
        self.registry.record_result(&record.id, &provenance).await?;
        Ok(provenance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema;
    use crate::worktree::domain::{BaselineMode, baseline_ref_for};
    use crate::worktree::git::CliGit;
    use crate::worktree::repo_workspace::SqliteWorkspaceRepository;
    use crate::worktree::test_support::{commit_files, init_repo, materialize};
    use rusqlite::Connection;
    use std::fs;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    #[tokio::test]
    async fn real_git_cross_feature_acceptance_retains_independent_work_and_resume() {
        use crate::session::backend::StorageBackend;
        use crate::snapshot::{GitSnapshotBackend, SnapshotBackend};
        use crate::verification::service::{VerificationContext, VerificationService};
        use crate::verification::{VerificationSpec, VerificationStep, VerificationStrategy};
        let repo = init_repo();
        commit_files(
            repo.path(),
            "init",
            &[("file.txt", "one"), ("result.txt", "original")],
        );
        materialize(
            repo.path(),
            &[("file.txt", "one"), ("result.txt", "original")],
        );
        let data = TempDir::new().unwrap();
        let db = data.path().join("sessions.db");
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(db.clone())
            .await
            .unwrap();
        let svc = WorkspaceService::new(
            Arc::new(CliGit::new()),
            storage.workspace_repository().unwrap(),
            data.path().join("worktrees"),
        );
        let store = storage.session_store();
        let owner = store.create_session(None, None, None, None).await.unwrap();
        let sibling_owner = store.create_session(None, None, None, None).await.unwrap();
        let parent = provision_owned(&svc, repo.path(), &owner.public_id).await;
        let sibling = provision_owned(&svc, repo.path(), &sibling_owner.public_id).await;
        let snapshots = GitSnapshotBackend::with_snapshot_base(data.path().join("snapshots"));
        let parent_before = snapshots.track(&parent.checkout_path).await.unwrap();
        fs::write(parent.checkout_path.join("file.txt"), "parent dirty").unwrap();
        let parent_after = snapshots.track(&parent.checkout_path).await.unwrap();
        let (child1, child2) = tokio::join!(
            svc.allocate_for_delegation(
                &owner.public_id,
                Some(&parent),
                &parent.checkout_path,
                None,
                "delegate-1",
                Some("acceptance".into())
            ),
            svc.allocate_for_delegation(
                &owner.public_id,
                Some(&parent),
                &parent.checkout_path,
                None,
                "delegate-2",
                Some("acceptance".into())
            ),
        );
        let child1 = child1.unwrap();
        let child2 = child2.unwrap();
        assert_eq!(child1.record.baseline, child2.record.baseline);
        let child_owner = store.create_session(None, None, None, None).await.unwrap();
        svc.bind_owner(&child1.record.id, &child_owner.public_id)
            .await
            .unwrap();
        for child in [&child1, &child2] {
            assert_eq!(
                fs::read_to_string(child.effective_cwd.join("file.txt")).unwrap(),
                "parent dirty"
            );
        }
        let child_before = snapshots.track(&child1.effective_cwd).await.unwrap();
        fs::write(child1.effective_cwd.join("result.txt"), "child1").unwrap();
        fs::write(child2.effective_cwd.join("result.txt"), "child2").unwrap();
        let child_after = snapshots.track(&child1.effective_cwd).await.unwrap();
        let verify = VerificationService::new(Arc::new(crate::tools::ToolRegistry::new()));
        let spec = VerificationSpec {
            description: "child only".into(),
            strategy: VerificationStrategy::All,
            steps: vec![VerificationStep::FileAssertion {
                path: "result.txt".into(),
                exists: true,
                contains: Some("child1".into()),
                matches_regex: None,
            }],
        };
        let context = |cwd: PathBuf| VerificationContext {
            session_id: child_owner.public_id.clone(),
            task_id: None,
            delegation_id: Some("delegate-1".into()),
            cwd: Some(cwd),
            tool_registry: Arc::new(crate::tools::ToolRegistry::new()),
        };
        assert!(
            verify
                .verify(&spec, &context(child1.effective_cwd.clone()))
                .await
                .is_ok()
        );
        assert!(
            verify
                .verify(&spec, &context(parent.checkout_path.clone()))
                .await
                .is_err()
        );
        snapshots
            .restore(&parent.checkout_path, &parent_before)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(child1.effective_cwd.join("file.txt")).unwrap(),
            "parent dirty"
        );
        snapshots
            .restore(&parent.checkout_path, &parent_after)
            .await
            .unwrap();
        snapshots
            .restore(&child1.effective_cwd, &child_before)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(parent.checkout_path.join("file.txt")).unwrap(),
            "parent dirty"
        );
        snapshots
            .restore(&child1.effective_cwd, &child_after)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(child2.effective_cwd.join("result.txt")).unwrap(),
            "child2"
        );
        assert_eq!(
            fs::read_to_string(sibling.checkout_path.join("result.txt")).unwrap(),
            "original"
        );
        let result = svc
            .capture_delegate_result(
                &child1.record,
                &child1.effective_cwd,
                Some(&child1.mutation_gate),
                Some(ResultVerificationStatus::Passed),
            )
            .await
            .unwrap();
        drop(svc);
        drop(store);
        drop(storage);
        let reopened = crate::session::sqlite_storage::SqliteStorage::connect(db)
            .await
            .unwrap();
        let resumed = WorkspaceService::new(
            Arc::new(CliGit::new()),
            reopened.workspace_repository().unwrap(),
            data.path().join("worktrees"),
        );
        let bound = resumed
            .resolve_existing_session(&child_owner.public_id, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bound.record.id, child1.record.id);
        assert_eq!(bound.effective_cwd, child1.effective_cwd);
        let retained = resumed
            .registry()
            .list_results(&bound.record.id)
            .await
            .unwrap();
        assert_eq!(retained[0].result_commit, result.result_commit);
        let delta = CliGit::new()
            .run(
                &bound.effective_cwd,
                &[
                    "diff",
                    "--name-only",
                    &result.baseline_commit,
                    result.result_commit.as_deref().unwrap(),
                ],
            )
            .await
            .unwrap()
            .stdout_str();
        assert_eq!(delta.trim(), "result.txt");
        snapshots
            .restore(&bound.effective_cwd, &child_before)
            .await
            .unwrap();
        snapshots
            .restore(&bound.effective_cwd, &child_after)
            .await
            .unwrap();
        assert!(bound.effective_cwd.join(".git").is_file());
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "one"
        );
        assert!(resumed.manager().remove(&bound.record).await.is_err());
    }

    #[tokio::test]
    #[ignore = "subprocess helper invoked by multiprocess_workspace_acceptance"]
    async fn multiprocess_workspace_helper() {
        use crate::session::backend::StorageBackend;
        let db = PathBuf::from(std::env::var_os("QMT_ACCEPT_DB").unwrap());
        let root = PathBuf::from(std::env::var_os("QMT_ACCEPT_ROOT").unwrap());
        let repo = PathBuf::from(std::env::var_os("QMT_ACCEPT_REPO").unwrap());
        let marker = PathBuf::from(std::env::var_os("QMT_ACCEPT_MARKER").unwrap());
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(db)
            .await
            .unwrap();
        let owner = storage
            .session_store()
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let svc = WorkspaceService::new(
            Arc::new(CliGit::new()),
            storage.workspace_repository().unwrap(),
            &root,
        );
        let bound = svc
            .allocate_for_new_session(
                &repo,
                &SessionCreationOptions::isolated_worktree(BaselineMode::CurrentWorkingTree),
            )
            .await
            .unwrap()
            .unwrap();
        svc.bind_owner(&bound.record.id, &owner.public_id)
            .await
            .unwrap();
        assert!(
            svc.bind_owner(&bound.record.id, "foreign-owner")
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(bound.effective_cwd.join("file.txt")).unwrap(),
            "parent dirty"
        );
        fs::write(bound.effective_cwd.join("file.txt"), "recoverable child").unwrap();
        let result = svc
            .capture_delegate_result(
                &bound.record,
                &bound.effective_cwd,
                Some(&bound.mutation_gate),
                Some(ResultVerificationStatus::Passed),
            )
            .await
            .unwrap();
        assert!(svc.manager().remove(&bound.record).await.is_err());
        svc.registry().detach_owner(&bound.record.id).await.unwrap();
        assert!(svc.manager().remove(&bound.record).await.is_err());
        assert_eq!(
            fs::read_to_string(bound.effective_cwd.join("file.txt")).unwrap(),
            "recoverable child"
        );
        fs::write(bound.effective_cwd.join("file.txt"), "parent dirty").unwrap();
        let removed = svc.manager().remove(&bound.record).await.unwrap();
        assert!(
            removed
                .retained_refs
                .contains(result.result_ref.as_ref().unwrap())
        );
        fs::write(marker, bound.record.id.as_str()).unwrap();
    }

    #[tokio::test]
    async fn multiprocess_workspace_acceptance() {
        use crate::session::backend::StorageBackend;
        let repo = init_repo();
        commit_files(repo.path(), "init", &[("file.txt", "one")]);
        materialize(repo.path(), &[("file.txt", "parent dirty")]);
        let data = TempDir::new().unwrap();
        let db = data.path().join("sessions.db");
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(db.clone())
            .await
            .unwrap();
        let mut processes = Vec::new();
        for n in 0..3 {
            let marker = data.path().join(format!("child-{n}"));
            let child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "worktree::service::tests::multiprocess_workspace_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("QMT_ACCEPT_DB", &db)
                .env("QMT_ACCEPT_ROOT", data.path().join("worktrees"))
                .env("QMT_ACCEPT_REPO", repo.path())
                .env("QMT_ACCEPT_MARKER", &marker)
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            processes.push((child, marker));
        }
        let mut ids = std::collections::HashSet::new();
        for (mut child, marker) in processes {
            let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(status.success());
            assert!(ids.insert(fs::read_to_string(marker).unwrap()));
        }
        let registry = storage.workspace_repository().unwrap();
        let records = registry.list_workspaces().await.unwrap();
        assert_eq!(records.len(), 3);
        for record in records {
            assert_eq!(record.lifecycle, WorkspaceLifecycle::Removed);
            assert!(record.ownership.owner_session_id.is_none());
            assert!(!record.checkout_path.exists());
            assert_eq!(registry.list_results(&record.id).await.unwrap().len(), 1);
        }
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "parent dirty"
        );
    }

    async fn service() -> (TempDir, TempDir, WorkspaceService) {
        let repo = init_repo();
        commit_files(repo.path(), "init", &[("file.txt", "one")]);
        materialize(repo.path(), &[("file.txt", "one")]);
        let storage = TempDir::new().unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        schema::init_schema(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO sessions (public_id, name, created_at, updated_at) \
             VALUES ('sess-owner', 'owner', '2026-01-15T00:00:00Z', '2026-01-15T00:00:00Z')",
            [],
        )
        .unwrap();
        let conn = Arc::new(StdMutex::new(conn));
        let svc = WorkspaceService::new(
            Arc::new(CliGit::new()),
            Arc::new(SqliteWorkspaceRepository::new(conn)),
            storage.path(),
        );
        (repo, storage, svc)
    }

    /// Provision a real managed workspace (with an on-disk checkout) and
    /// bind `session_id` as its writable owner.
    async fn provision_owned(
        svc: &WorkspaceService,
        repo_root: &Path,
        session_id: &str,
    ) -> WorkspaceRecord {
        let binding = svc
            .allocate_for_new_session(
                repo_root,
                &SessionCreationOptions::isolated_worktree(BaselineMode::Commit),
            )
            .await
            .unwrap()
            .expect("isolation allocates");
        svc.bind_owner(&binding.record.id, session_id)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn local_options_allocate_nothing() {
        let (_repo, _storage, svc) = service().await;
        let binding = svc
            .allocate_for_new_session(Path::new("/repo"), &SessionCreationOptions::local())
            .await
            .unwrap();
        assert!(binding.is_none(), "local execution creates no workspace");
    }

    /// Delegation isolation inheritance follows the parent's persisted
    /// workspace binding: unbound sessions are local, bound sessions are
    /// isolated workspaces.
    #[tokio::test]
    async fn session_execution_mode_reflects_the_workspace_binding() {
        let (repo, _storage, svc) = service().await;

        // Unbound sessions keep legacy local execution.
        let mode = svc.session_execution_mode("sess-owner").await.unwrap();
        assert_eq!(mode, crate::worktree::ExecutionMode::Local);

        provision_owned(&svc, repo.path(), "sess-owner").await;
        let mode = svc.session_execution_mode("sess-owner").await.unwrap();
        assert_eq!(mode, crate::worktree::ExecutionMode::Worktree);
    }

    /// Concurrent same-path edits in delegation siblings: both children
    /// start from the parent's (shared) baseline, get distinct checkouts,
    /// and edit the same relative path without touching the parent or each
    /// other.
    #[tokio::test]
    async fn delegation_siblings_share_baseline_and_edit_independently() {
        let (repo, _storage, svc) = service().await;

        // Isolated parent with uncommitted work and an untracked file.
        let parent = provision_owned(&svc, repo.path(), "sess-owner").await;
        fs::write(parent.checkout_path.join("file.txt"), "parent edit").unwrap();
        fs::write(parent.checkout_path.join("notes.txt"), "untracked").unwrap();

        let child1 = svc
            .allocate_for_delegation(
                "sess-owner",
                Some(&parent),
                &parent.checkout_path,
                None,
                "delegation-1",
                Some("batch-1".to_string()),
            )
            .await
            .unwrap();
        fs::write(parent.checkout_path.join("file.txt"), "later parent edit").unwrap();
        let child2 = svc
            .allocate_for_delegation(
                "sess-owner",
                Some(&parent),
                &parent.checkout_path,
                None,
                "delegation-2",
                Some("batch-1".to_string()),
            )
            .await
            .unwrap();

        // Siblings share the parent's persisted baseline but never a path.
        let baseline1 = child1.record.baseline.as_ref().unwrap();
        let baseline2 = child2.record.baseline.as_ref().unwrap();
        assert_eq!(baseline1.baseline_commit, baseline2.baseline_commit);
        assert_ne!(child1.record.checkout_path, child2.record.checkout_path);

        // Both children start from the parent's dirty working contents.
        assert_eq!(
            fs::read_to_string(child1.effective_cwd.join("file.txt")).unwrap(),
            "parent edit"
        );
        assert_eq!(
            fs::read_to_string(child2.effective_cwd.join("notes.txt")).unwrap(),
            "untracked"
        );

        assert_eq!(
            fs::read_to_string(child2.effective_cwd.join("file.txt")).unwrap(),
            "parent edit",
            "batch siblings reuse the original baseline despite parent edits"
        );
        // Concurrent same-path edits stay isolated per checkout.
        fs::write(child1.effective_cwd.join("file.txt"), "child1 edit").unwrap();
        fs::write(child2.effective_cwd.join("file.txt"), "child2 edit").unwrap();
        assert_eq!(
            fs::read_to_string(child1.effective_cwd.join("file.txt")).unwrap(),
            "child1 edit"
        );
        assert_eq!(
            fs::read_to_string(child2.effective_cwd.join("file.txt")).unwrap(),
            "child2 edit"
        );

        // Allocation and sibling edits leave the newer parent state untouched.
        assert_eq!(
            fs::read_to_string(parent.checkout_path.join("file.txt")).unwrap(),
            "later parent edit"
        );
        let later = svc
            .allocate_for_delegation(
                "sess-owner",
                Some(&parent),
                &parent.checkout_path,
                None,
                "delegation-later",
                Some("batch-later".into()),
            )
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(later.effective_cwd.join("file.txt")).unwrap(),
            "later parent edit"
        );

        // Parent/batch/baseline association persisted at allocation time.
        for (child, delegation_id) in [&child1, &child2]
            .into_iter()
            .zip(["delegation-1", "delegation-2"])
        {
            let stored = svc
                .registry()
                .get_workspace(&child.record.id)
                .await
                .unwrap();
            let stored = stored.expect("child workspace registered");
            assert_eq!(
                stored
                    .ownership
                    .parent_workspace_id
                    .as_ref()
                    .map(|id| id.as_str()),
                Some(parent.id.as_str())
            );
            assert_eq!(
                stored.ownership.parent_session_id.as_deref(),
                Some("sess-owner")
            );
            assert_eq!(
                stored.ownership.delegation_id.as_deref(),
                Some(delegation_id)
            );
            assert_eq!(
                stored.ownership.delegation_batch_id.as_deref(),
                Some("batch-1")
            );
        }
    }

    /// Nested delegation: a delegate that delegates further gets a workspace
    /// whose parent link points at its own (delegate) workspace.
    #[tokio::test]
    async fn nested_delegation_links_child_to_its_delegate_parent() {
        let (repo, _storage, svc) = service().await;

        let parent = provision_owned(&svc, repo.path(), "sess-owner").await;
        fs::write(parent.checkout_path.join("file.txt"), "parent edit").unwrap();

        let delegate = svc
            .allocate_for_delegation(
                "sess-owner",
                Some(&parent),
                &parent.checkout_path,
                None,
                "delegation-1",
                None,
            )
            .await
            .unwrap();

        // The delegate (itself isolated) delegates further: the nested
        // child links to the delegate's workspace.
        let nested = svc
            .allocate_for_delegation(
                "sess-owner",
                Some(&delegate.record),
                &delegate.effective_cwd,
                None,
                "delegation-2",
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            nested
                .record
                .ownership
                .parent_workspace_id
                .as_ref()
                .map(|id| id.as_str()),
            Some(delegate.record.id.as_str()),
            "nested child links to the delegate's workspace"
        );
        assert_ne!(
            nested.record.checkout_path, delegate.record.checkout_path,
            "nested delegation never shares the delegate's checkout"
        );
        assert_eq!(
            fs::read_to_string(nested.effective_cwd.join("file.txt")).unwrap(),
            "parent edit"
        );
    }

    /// Result capture preserves the delegate's delta: inherited parent
    /// edits (already in the baseline) are not misattributed to the child,
    /// result publication leaves the child's staging state and the parent
    /// checkout unchanged, and the provenance is durably retained.
    #[tokio::test]
    async fn delegate_result_capture_is_baseline_relative_and_non_invasive() {
        use crate::worktree::{CliGit, ResultVerificationStatus};

        let (repo, _storage, svc) = service().await;
        let git = CliGit::new();

        // Parent with an inherited edit and an untracked file; both are
        // part of the delegate's baseline, not the delegate's work.
        let parent = provision_owned(&svc, repo.path(), "sess-owner").await;
        fs::write(parent.checkout_path.join("file.txt"), "parent edit").unwrap();
        fs::write(parent.checkout_path.join("notes.txt"), "untracked").unwrap();

        let child = svc
            .allocate_for_delegation(
                "sess-owner",
                Some(&parent),
                &parent.checkout_path,
                None,
                "delegation-1",
                None,
            )
            .await
            .unwrap();
        let baseline_commit = child
            .record
            .baseline
            .as_ref()
            .unwrap()
            .baseline_commit
            .clone();

        // The delegate stages a real change to a tracked file and edits one
        // new file without staging it. notes.txt and out.txt are the
        // delegate's own work; file.txt is the inherited parent edit.
        fs::write(child.effective_cwd.join("notes.txt"), "child staged edit").unwrap();
        git.run(child.effective_cwd.as_path(), &["add", "notes.txt"])
            .await
            .unwrap()
            .into_checked(&["add"])
            .unwrap();
        fs::write(child.effective_cwd.join("out.txt"), "delegate work").unwrap();

        let provenance = svc
            .capture_delegate_result(
                &child.record,
                &child.effective_cwd,
                Some(&child.mutation_gate),
                Some(ResultVerificationStatus::Passed),
            )
            .await
            .unwrap();

        // Provenance is baseline-relative and durably retained.
        assert_eq!(provenance.baseline_commit, baseline_commit);
        let result_commit = provenance.result_commit.as_deref().unwrap();
        let result_ref = provenance.result_ref.as_deref().unwrap();
        let pinned = git
            .run(child.effective_cwd.as_path(), &["rev-parse", result_ref])
            .await
            .unwrap()
            .stdout_str();
        assert_eq!(pinned.trim(), result_commit, "result ref pins the commit");
        let stored = svc.registry().list_results(&child.record.id).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].verification,
            Some(ResultVerificationStatus::Passed)
        );

        // Publication leaves the child's real staging state unchanged:
        // the staged notes.txt edit stays staged, out.txt stays untracked.
        let status = git
            .run(child.effective_cwd.as_path(), &["status", "--porcelain"])
            .await
            .unwrap()
            .stdout_str();
        assert!(status.contains("M  notes.txt"), "status: {status}");
        assert!(status.contains("?? out.txt"), "status: {status}");

        // The parent checkout is untouched by result publication.
        assert_eq!(
            fs::read_to_string(parent.checkout_path.join("file.txt")).unwrap(),
            "parent edit"
        );
        assert_eq!(
            fs::read_to_string(parent.checkout_path.join("notes.txt")).unwrap(),
            "untracked"
        );

        // Baseline-relative diff shows only the delegate's own delta —
        // the inherited parent edit to file.txt is not misattributed.
        let changed = git
            .run(
                child.effective_cwd.as_path(),
                &[
                    "diff",
                    "--name-only",
                    baseline_commit.as_str(),
                    result_commit,
                ],
            )
            .await
            .unwrap()
            .stdout_str();
        let mut files: Vec<&str> = changed.lines().map(str::trim).collect();
        files.retain(|f| !f.is_empty());
        files.sort_unstable();
        assert_eq!(files, vec!["notes.txt", "out.txt"], "changed: {changed}");
        fs::write(child.effective_cwd.join("out.txt"), "second result").unwrap();
        let second = svc
            .capture_delegate_result(
                &child.record,
                &child.effective_cwd,
                Some(&child.mutation_gate),
                Some(ResultVerificationStatus::Skipped),
            )
            .await
            .unwrap();
        assert_ne!(second.result_ref.as_deref(), Some(result_ref));
        assert_eq!(
            git.run(&child.effective_cwd, &["rev-parse", result_ref])
                .await
                .unwrap()
                .stdout_str()
                .trim(),
            result_commit,
            "new publication never overwrites old refs"
        );
        assert_eq!(
            svc.registry()
                .list_results(&child.record.id)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    /// Undo compatibility: the worktree service must be fully usable with
    /// snapshots disabled. This test constructs no `SnapshotBackend` at all
    /// — the service API carries no snapshot dependency, so the complete
    /// allocate → bind → status → remove lifecycle works without one.
    #[tokio::test]
    async fn worktree_lifecycle_without_snapshot_backend() {
        let (repo, _storage, svc) = service().await;

        let record = provision_owned(&svc, repo.path(), "sess-owner").await;

        let status = svc.manager().status(&record).await.unwrap();
        assert_eq!(status.lifecycle, crate::worktree::WorkspaceLifecycle::Ready);
        assert!(status.checkout_exists);
        assert!(status.registered);
        assert!(status.is_clean_for_removal());

        // Conservative removal still applies without a snapshot backend: an
        // active owner blocks removal.
        let err = svc.manager().remove(&record).await.unwrap_err();
        assert!(err.to_string().contains("active"), "error: {err}");

        svc.registry().detach_owner(&record.id).await.unwrap();
        let report = svc.manager().remove(&record).await.unwrap();
        assert!(
            report.retained_refs.contains(&baseline_ref_for(&record.id)),
            "removal retains the workspace baseline ref: {:?}",
            report.retained_refs
        );
    }

    #[tokio::test]
    async fn isolated_options_allocate_and_bind_a_single_owner() {
        let (repo, _storage, svc) = service().await;

        let draft = svc
            .allocate_for_new_session(
                repo.path(),
                &SessionCreationOptions::isolated_worktree(BaselineMode::Commit),
            )
            .await
            .unwrap()
            .expect("isolation allocates");

        assert_eq!(draft.record.lifecycle, WorkspaceLifecycle::Ready);
        assert!(draft.record.ownership.owner_session_id.is_none());
        assert!(draft.effective_cwd.starts_with(&draft.record.checkout_path));
        let canonical_checkout = std::fs::canonicalize(&draft.record.checkout_path).unwrap();
        assert_eq!(
            draft.mutation_gate.checkout_root(),
            canonical_checkout,
            "the gate serializes the workspace checkout"
        );

        // Bind; a second writable owner is refused.
        let bound = svc
            .bind_owner(&draft.record.id, "sess-owner")
            .await
            .unwrap();
        assert_eq!(
            bound.ownership.owner_session_id.as_deref(),
            Some("sess-owner")
        );
        let err = svc
            .bind_owner(&draft.record.id, "sess-other")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("owner"), "error: {err}");
    }

    #[tokio::test]
    async fn invalid_options_fail_closed() {
        let (repo, _storage, svc) = service().await;
        let options = SessionCreationOptions::isolated_worktree(BaselineMode::CurrentWorkingTree)
            .with_base_ref("v1");
        let err = svc
            .allocate_for_new_session(repo.path(), &options)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be combined"));
    }

    #[tokio::test]
    async fn load_resolves_the_persisted_binding() {
        let (repo, _storage, svc) = service().await;
        let record = provision_owned(&svc, repo.path(), "sess-owner").await;

        let binding = svc
            .resolve_existing_session("sess-owner", None)
            .await
            .unwrap()
            .expect("bound session resolves");
        assert_eq!(binding.record.id, record.id);
        assert_eq!(binding.effective_cwd, record.session_cwd());
    }

    #[tokio::test]
    async fn conflicting_cwd_is_rejected_for_bound_sessions() {
        let (repo, _storage, svc) = service().await;
        provision_owned(&svc, repo.path(), "sess-owner").await;

        let err = svc
            .resolve_existing_session("sess-owner", Some(Path::new("/some/other/place")))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("conflicting cwd"), "error: {message}");
    }

    #[tokio::test]
    async fn missing_checkout_reconciles_and_fails_closed() {
        let (repo, _storage, svc) = service().await;
        let record = provision_owned(&svc, repo.path(), "sess-owner").await;
        // Simulate external deletion of the checkout.
        fs::remove_dir_all(&record.checkout_path).unwrap();

        let err = svc
            .resolve_existing_session("sess-owner", None)
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("missing"), "error: {message}");
        assert!(message.contains("falling back"), "error: {message}");

        // The durable lifecycle now records the missing checkout.
        let stored = svc
            .registry()
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Missing);
    }

    #[tokio::test]
    async fn unready_lifecycles_reject_attachment() {
        let (repo, _storage, svc) = service().await;
        let record = provision_owned(&svc, repo.path(), "sess-owner").await;
        svc.registry()
            .transition_lifecycle(
                &record.id,
                WorkspaceLifecycle::Ready,
                WorkspaceLifecycle::Failed,
            )
            .await
            .unwrap();

        let err = svc
            .resolve_existing_session("sess-owner", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("cannot be attached"),
            "error: {err}"
        );
    }

    #[tokio::test]
    async fn legacy_local_sessions_resolve_to_none() {
        let (_repo, _storage, svc) = service().await;
        let binding = svc
            .resolve_existing_session("sess-unknown", None)
            .await
            .unwrap();
        assert!(binding.is_none(), "unbound sessions keep local behavior");
    }

    #[test]
    fn baseline_refs_are_namespaced_per_workspace() {
        let id = WorkspaceId::from_raw("ws-9");
        assert_eq!(baseline_ref_for(&id), "refs/qmt/baselines/ws-9");
    }
}
