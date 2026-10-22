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
    Baseline, BaselineMode, ResultProvenance, ResultReview, ResultVerificationStatus, WorkspaceId,
    WorkspaceLifecycle, WorkspaceRecord,
};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::gate::CheckoutMutationGate;
use crate::worktree::identity::resolve_repository;
use crate::worktree::lifecycle::WorkspaceManager;
use crate::worktree::repo_ops::RepositoryOps;
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

impl WorkspaceBinding {
    /// Disclosure of the resolved binding for a `session/new` response
    /// (`_meta.querymt.sessionWorkspace`): the actual execution cwd, workspace
    /// id, branch, baseline mode/OID and what was excluded.
    pub fn disclosure(&self) -> serde_json::Value {
        let mode = self
            .record
            .baseline
            .as_ref()
            .map_or(BaselineMode::Commit, |b| b.mode);
        serde_json::json!({
            "execution": "worktree",
            "workspaceId": self.record.id.as_str(),
            "sessionCwd": self.effective_cwd.display().to_string(),
            "checkoutPath": self.record.checkout_path.display().to_string(),
            "branch": self.record.branch.as_str(),
            "baseline": mode.to_string(),
            "baselineCommit": self.record.baseline.as_ref().map(|b| b.baseline_commit.clone()),
            "localChangesExcluded": mode == BaselineMode::Commit,
            "ignoredUntrackedFilesExcluded": true,
        })
    }
}

/// Facade over managed workspace operations for the session layer.
pub struct WorkspaceService {
    git: Arc<dyn RepositoryOps>,
    allocator: WorkspaceAllocator,
    manager: WorkspaceManager,
    registry: Arc<dyn WorkspaceRepository>,
    storage_root: PathBuf,
    default_baseline: BaselineMode,
}

impl WorkspaceService {
    pub fn new(
        git: Arc<dyn RepositoryOps>,
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
            default_baseline: BaselineMode::Commit,
        }
    }

    /// Set the baseline used for explicitly requested top-level isolation that
    /// does not choose one. Delegates and forks are unaffected.
    #[must_use]
    pub fn with_default_baseline(mut self, baseline: BaselineMode) -> Self {
        self.default_baseline = baseline;
        self
    }

    /// Default baseline for explicit top-level isolation requests.
    pub fn default_baseline(&self) -> BaselineMode {
        self.default_baseline
    }

    /// Capability advertisement for `initialize` (`_meta.querymt.workspace`),
    /// so clients can offer an explicit Local/Worktree choice at creation.
    pub fn capability_meta(&self) -> serde_json::Value {
        serde_json::json!({
            "supported": true,
            "defaultBaseline": self.default_baseline.to_string(),
            "executionModes": ["local", "worktree"],
            "baselines": ["commit", "current_working_tree"],
        })
    }

    /// Build the single workspace service described by `settings`.
    ///
    /// Returns `Ok(None)` when the service is disabled, so callers can thread
    /// the result straight into a builder. Everything that can be wrong with
    /// the settings is reported here, at build time, instead of at the first
    /// isolated session: the backend must expose a workspace repository, the
    /// storage root must be creatable and a directory, and it must be on the
    /// local filesystem.
    pub fn from_settings(
        settings: &crate::config::WorkspaceSettings,
        storage: &dyn crate::session::backend::StorageBackend,
    ) -> WorktreeResult<Option<Arc<Self>>> {
        if !settings.enabled {
            return Ok(None);
        }
        let registry = storage.workspace_repository().ok_or_else(|| {
            WorktreeError::InvalidOperation(
                "workspace isolation is enabled but the storage backend does not expose a \
                 workspace repository"
                    .to_string(),
            )
        })?;
        let root = settings
            .resolved_storage_root()
            .map_err(|err| WorktreeError::InvalidOperation(format!("{err:#}")))?;
        crate::worktree::placement::ensure_local_filesystem_execution(&root)?;
        std::fs::create_dir_all(&root).map_err(|err| {
            WorktreeError::InvalidOperation(format!(
                "workspace storage_root {} is unusable: {err}",
                root.display()
            ))
        })?;
        Ok(Some(Arc::new(
            Self::new(Arc::new(crate::worktree::GixRepoOps::new()), registry, root)
                .with_default_baseline(settings.baseline),
        )))
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

    /// Typed repository operations backing this service.
    pub fn ops(&self) -> &Arc<dyn RepositoryOps> {
        &self.git
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
        // Serialize working-tree capture against cooperating writers of the
        // source checkout (local sessions install this same gate at actor
        // startup), exactly as delegation does.
        let source_gate = CheckoutMutationGate::new(&self.storage_root, &resolved.checkout.root)?;
        let record = self
            .allocator
            .allocate_with_baseline(
                &resolved,
                &relative,
                options.base_ref.as_deref(),
                options.baseline,
                Some(&source_gate),
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

    /// Compensation for a failed owner binding: mark a freshly allocated,
    /// never-owned workspace `Failed` with the binding diagnostic.
    ///
    /// The workspace is unused by construction — no session was ever bound —
    /// so this is purely bookkeeping: Git resources (branch, checkout,
    /// baseline ref) are retained for the explicit recovery/removal surface
    /// instead of being deleted here. The record becomes visibly failed and
    /// can never be attached as-is afterwards.
    ///
    /// A workspace that already has an owner is left untouched (there is
    /// nothing unused to compensate), as is an unknown id.
    pub async fn mark_owner_binding_failed(
        &self,
        workspace_id: &WorkspaceId,
        diagnostic: &str,
    ) -> WorktreeResult<()> {
        let Some(mut record) = self.registry.get_workspace(workspace_id).await? else {
            return Ok(());
        };
        if record.ownership.owner_session_id.is_some() {
            return Ok(());
        }
        if self
            .registry
            .transition_lifecycle(
                workspace_id,
                WorkspaceLifecycle::Ready,
                WorkspaceLifecycle::Failed,
            )
            .await?
        {
            record.lifecycle = WorkspaceLifecycle::Failed;
        }
        record.last_error = Some(diagnostic.to_string());
        self.registry.update_workspace(record).await?;
        Ok(())
    }

    /// Explicit recovery for a pending isolated fork: complete provisioning
    /// for `fork_session_id` from its persisted intent and bind the fork as
    /// the single writable owner.
    ///
    /// Duplicate-owner-safe by construction:
    /// - an existing binding already owned by this session is reused
    ///   verbatim (the retry resumes after a crash between binding and
    ///   barrier release);
    /// - otherwise a fresh workspace is allocated. Resources from prior
    ///   failed attempts remain as explicit failed/unowned records and are
    ///   never reowned; the unique-owner index guarantees this session owns
    ///   at most one workspace, so retries cannot create duplicate owners.
    ///
    /// The baseline of a fresh attempt is captured at recovery time from the
    /// recorded source; the caller must disclose that it reflects
    /// recovery-time contents. After this returns, the caller persists the
    /// execution cwd and releases the startup barrier
    /// ([`crate::session::store::SessionStore::complete_fork_isolation`]).
    pub async fn recover_fork_isolation(
        &self,
        fork_session_id: &str,
        intent: &crate::session::store::PendingForkIsolation,
    ) -> WorktreeResult<WorkspaceBinding> {
        let options = SessionCreationOptions::from_workspace_options(&intent.options);
        options
            .validate()
            .map_err(WorktreeError::InvalidOperation)?;
        if !options.requests_isolation() {
            return Err(WorktreeError::InvalidOperation(
                "pending fork isolation intent does not request a worktree; refusing \
                 to publish it as a local session"
                    .to_string(),
            ));
        }

        if let Some(record) = self
            .registry
            .get_workspace_by_session(fork_session_id)
            .await?
        {
            if record.lifecycle != WorkspaceLifecycle::Ready {
                return Err(WorktreeError::InvalidOperation(format!(
                    "workspace {} for session {fork_session_id} is {} and cannot be attached",
                    record.id, record.lifecycle
                )));
            }
            return WorkspaceBinding::new(record, &self.storage_root);
        }

        let binding = self
            .allocate_for_fork(&intent.source_cwd, &options, None)
            .await?;
        if let Err(err) = self.bind_owner(&binding.record.id, fork_session_id).await {
            let _ = self
                .mark_owner_binding_failed(
                    &binding.record.id,
                    &format!("fork owner binding failed during recovery: {err}"),
                )
                .await;
            return Err(err);
        }
        Ok(binding)
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
        // A managed source supplies its own gate; a local (unmanaged) source
        // falls back to a gate on its resolved checkout so capture is still
        // serialized against cooperating writers.
        let fallback_gate;
        let source_gate = match source_gate {
            Some(gate) => gate,
            None => {
                fallback_gate =
                    CheckoutMutationGate::new(&self.storage_root, &resolved.checkout.root)?;
                &fallback_gate
            }
        };
        let record = self
            .allocator
            .allocate_with_baseline(
                &resolved,
                &relative,
                options.base_ref.as_deref(),
                options.baseline,
                Some(source_gate),
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

    /// Resolve a session's effective execution cwd.
    ///
    /// This is the single authoritative resolution path for every consumer
    /// of a session's working directory — delegation dispatch, native
    /// worktree creation, verification/hooks, and undo child filtering must
    /// resolve cwd through here instead of reading the persisted session row
    /// directly. The durable workspace binding wins: sessions whose row cwd
    /// was copied from a source checkout (e.g. isolated forks before their
    /// row is rewritten) still resolve to their own bound checkout.
    /// Unbound legacy local sessions fall back to the supplied persisted cwd.
    ///
    /// Bound sessions fail closed: resolution errors (non-`Ready`
    /// lifecycle, missing checkout) must never be exchanged for a shared or
    /// original-checkout fallback.
    pub async fn resolve_session_cwd(
        &self,
        session_id: &str,
        persisted_cwd: Option<std::path::PathBuf>,
    ) -> WorktreeResult<Option<std::path::PathBuf>> {
        match self.resolve_existing_session(session_id, None).await? {
            Some(binding) => Ok(Some(binding.effective_cwd)),
            None => Ok(persisted_cwd),
        }
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
                    // Repository-wide capture runs from the resolved checkout
                    // root; the parent's relative cwd only affects where the
                    // delegate executes.
                    self.capture_batch_baseline(
                        &resolved.checkout.root,
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

    /// Publish the uncommitted working state of a workspace checkout as an
    /// immutable, reviewable result (design decision 5). This is the single
    /// publication/review path for delegates **and** top-level workspaces;
    /// the explicit top-level publication operation uses it with verification
    /// state `Skipped` (no verification is performed for publication).
    ///
    /// Reuses the repository capture primitive (a private temporary index),
    /// so the checkout's real staging state is untouched and no commit hooks
    /// run. The result commit's tree is the full working state at capture
    /// time; diffing it against the workspace baseline yields exactly the
    /// delta — for delegates, inherited parent edits are part of the baseline
    /// and are never misattributed to the child. The result is pinned by a
    /// unique create-only retained ref in the shared repository; no other
    /// checkout is touched.
    ///
    /// # Errors
    /// Returns an explicit error when execution is not local, capture
    /// validation fails, or the retained ref cannot be written. The checkout
    /// and its index are left unchanged by a failed publication; a partially
    /// written ref is never silently replaced (create-only semantics).
    pub async fn publish_result(
        &self,
        record: &WorkspaceRecord,
        checkout: &Path,
        gate: Option<&CheckoutMutationGate>,
        verification: Option<ResultVerificationStatus>,
    ) -> WorktreeResult<ResultProvenance> {
        crate::worktree::placement::ensure_local_filesystem_execution(checkout)?;

        let capture = capture_working_state_validated(
            self.git.as_ref(),
            checkout,
            CaptureRetryPolicy::default(),
        );
        let captured = match gate {
            Some(gate) => gate.run(capture).await??,
            None => capture.await?,
        };

        // Unique create-only refs preserve results even after partial
        // publication. The ref is written through the record's canonical
        // common directory — the administration handle — never through the
        // checkout path.
        let result_ref = format!("refs/qmt/results/{}/{}", record.id, uuid::Uuid::now_v7());
        self.git
            .create_ref_if_absent(
                &record.repository.common_dir,
                result_ref.as_str(),
                &captured.commit,
            )
            .await?;

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
            captured_head: Some(captured.source_head),
        };
        self.registry.record_result(&record.id, &provenance).await?;
        Ok(provenance)
    }

    /// Build the immutable baseline-relative review for one retained result.
    ///
    /// Changed paths and change kinds are diffed between the pinned
    /// baseline and result commits through the canonical common directory —
    /// never from live working-tree state — so review is identical after
    /// later edits, HEAD movement, restart, or snapshot GC. Provenance rows
    /// persisted before `captured_head` existed report the workspace
    /// baseline commit in its place.
    ///
    /// # Errors
    /// Returns an explicit error when the result was not captured
    /// (`result_commit` absent), a pinned object is missing from the
    /// repository, or the diff cannot be computed.
    pub async fn review_result(
        &self,
        record: &WorkspaceRecord,
        provenance: &ResultProvenance,
    ) -> WorktreeResult<ResultReview> {
        let result_commit = provenance.result_commit.as_deref().ok_or_else(|| {
            WorktreeError::InvalidOperation(
                "result was not captured; there is no immutable result to review".to_string(),
            )
        })?;
        let changed_paths = self
            .git
            .changed_paths_between(
                &record.repository.common_dir,
                &provenance.baseline_commit,
                result_commit,
            )
            .await?;
        Ok(ResultReview {
            baseline_commit: provenance.baseline_commit.clone(),
            result_commit: result_commit.to_string(),
            result_ref: provenance.result_ref.clone(),
            captured_head: provenance
                .captured_head
                .clone()
                .unwrap_or_else(|| provenance.baseline_commit.clone()),
            verification: provenance.verification,
            captured_at: provenance.captured_at,
            changed_paths,
            baseline_includes_uncommitted_edits: record
                .baseline
                .as_ref()
                .is_some_and(|b| b.mode == BaselineMode::CurrentWorkingTree),
        })
    }

    /// Review the newest retained result of a workspace, if any.
    ///
    /// Inspection never publishes: absence returns `None` instead of
    /// silently recapturing live state.
    ///
    /// # Errors
    /// Propagates registry and diff errors from [`Self::review_result`].
    pub async fn review_latest_result(
        &self,
        record: &WorkspaceRecord,
    ) -> WorktreeResult<Option<ResultReview>> {
        let results = self.registry.list_results(&record.id).await?;
        match results.into_iter().find(|r| r.result_commit.is_some()) {
            Some(provenance) => self.review_result(record, &provenance).await.map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema;
    use crate::worktree::GitRunner as _;
    use crate::worktree::GixRepoOps;
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
            Arc::new(GixRepoOps::new()),
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
            .publish_result(
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
            Arc::new(GixRepoOps::new()),
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
            Arc::new(GixRepoOps::new()),
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
            .publish_result(
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

    /// Every production step of the workspace lifecycle must work when the
    /// Git executable is unavailable: creation, private working-state
    /// capture, immutable publication/inspection, provisioning and removal
    /// recovery, and conservative removal. Fixtures are built with the typed
    /// gix API (no Git CLI), and `PATH` is emptied for the duration of the
    /// test so any accidental subprocess spawn would fail loudly.
    ///
    /// The empty `PATH` is set only on a child process (a re-exec of this
    /// test binary running [`no_git_lifecycle_child`]), so the parent
    /// process environment is never mutated while other tests run in
    /// parallel threads.
    #[test]
    fn production_lifecycle_works_without_a_git_executable() {
        let empty_path = TempDir::new().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worktree::service::tests::no_git_lifecycle_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("QMT_NO_GIT_CHILD", "1")
            .env("PATH", empty_path.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "lifecycle must work without a Git executable:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child probe must actually run: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// Child half of [`production_lifecycle_works_without_a_git_executable`];
    /// a no-op unless launched by it with an empty `PATH`.
    #[tokio::test]
    async fn no_git_lifecycle_child() {
        if std::env::var_os("QMT_NO_GIT_CHILD").is_none() {
            return;
        }
        use crate::session::backend::StorageBackend;
        use crate::worktree::{
            BaselineSource, ManagedBranch, ProvisioningPhase, WorkspaceAllocator, WorkspaceId,
            checkout_path_for, resolve_repository,
        };

        // Fixtures first: the repository is fully materialized before Git is
        // made unavailable.
        let repo = init_repo();
        commit_files(repo.path(), "init", &[("file.txt", "one")]);
        materialize(repo.path(), &[("file.txt", "parent dirty")]);

        // A baseline-absent provisioning intent, as a process that died after
        // persisting intent would leave behind. Recovery must be able to
        // replay it without a Git executable.
        let data = TempDir::new().unwrap();
        let db = data.path().join("sessions.db");
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(db.clone())
            .await
            .unwrap();
        let registry = storage.workspace_repository().unwrap();
        let storage_root = data.path().join("worktrees");
        let resolved = resolve_repository(repo.path()).unwrap();
        let id = WorkspaceId::generate();
        let intent = WorkspaceRecord {
            id: id.clone(),
            checkout_path: checkout_path_for(&storage_root, &resolved.identity.common_dir, &id),
            branch: ManagedBranch::for_workspace(&id),
            repository: resolved.identity.clone(),
            relative_cwd: PathBuf::new(),
            execution: crate::worktree::ExecutionMode::Worktree,
            baseline: None,
            lifecycle: WorkspaceLifecycle::Provisioning,
            ownership: crate::worktree::WorkspaceOwnership::default(),
            last_error: None,
            requested_baseline_mode: BaselineMode::CurrentWorkingTree,
            requested_base_ref: None,
            operation_id: "op-interrupted".into(),
            provisioning_phase: Some(ProvisioningPhase::IntentPersisted),
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
        };
        registry.insert_workspace(intent).await.unwrap();

        // From here on no Git executable is reachable. Negative control: a
        // plain `git --version` must fail, so the lifecycle below cannot
        // accidentally succeed by shelling out.
        assert!(
            std::process::Command::new("git")
                .arg("--version")
                .output()
                .is_err(),
            "test invariant: Git must be unreachable while PATH is hidden"
        );

        let allocator =
            WorkspaceAllocator::new(Arc::new(GixRepoOps::new()), registry.clone(), &storage_root);
        let pending = registry
            .get_workspace(&id)
            .await
            .unwrap()
            .expect("intent persists");
        assert!(pending.baseline.is_none());
        assert_eq!(pending.lifecycle, WorkspaceLifecycle::Provisioning);
        let recovered = allocator.recover(&pending).await.unwrap();
        assert_eq!(recovered.record.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(
            recovered.baseline_source,
            BaselineSource::CapturedDuringRecovery
        );
        assert_eq!(
            fs::read_to_string(recovered.record.checkout_path.join("file.txt")).unwrap(),
            "parent dirty"
        );

        // Fresh creation, capture, publication and inspection.
        let svc = WorkspaceService::new(
            Arc::new(GixRepoOps::new()),
            storage.workspace_repository().unwrap(),
            data.path().join("worktrees"),
        );
        let owner = storage
            .session_store()
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let bound = svc
            .allocate_for_new_session(
                repo.path(),
                &SessionCreationOptions::isolated_worktree(BaselineMode::CurrentWorkingTree),
            )
            .await
            .unwrap()
            .expect("isolation allocates");
        svc.bind_owner(&bound.record.id, &owner.public_id)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(bound.effective_cwd.join("file.txt")).unwrap(),
            "parent dirty"
        );
        fs::write(bound.effective_cwd.join("file.txt"), "child edit").unwrap();
        let result = svc
            .publish_result(
                &bound.record,
                &bound.effective_cwd,
                Some(&bound.mutation_gate),
                Some(ResultVerificationStatus::Passed),
            )
            .await
            .unwrap();
        let review = svc.review_result(&bound.record, &result).await.unwrap();
        assert_eq!(review.changed_paths.len(), 1);
        assert_eq!(review.changed_paths[0].path, "file.txt");
        assert_eq!(
            review.changed_paths[0].kind,
            crate::worktree::ChangeKind::Modified
        );

        // Conservative removal: unpreserved edits are refused, then a clean
        // checkout is removed with retained refs after archiving.
        svc.registry().detach_owner(&bound.record.id).await.unwrap();
        let current = svc
            .registry()
            .get_workspace(&bound.record.id)
            .await
            .unwrap()
            .unwrap();
        assert!(svc.manager().remove(&current).await.is_err());
        assert_eq!(
            fs::read_to_string(bound.effective_cwd.join("file.txt")).unwrap(),
            "child edit"
        );
        fs::write(bound.effective_cwd.join("file.txt"), "parent dirty").unwrap();
        svc.manager().archive(&current).await.unwrap();
        let archived = svc
            .registry()
            .get_workspace(&bound.record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archived.lifecycle, WorkspaceLifecycle::Archived);
        let removed = svc.manager().remove(&archived).await.unwrap();
        assert!(removed.checkout_removed);
        assert!(
            removed
                .retained_refs
                .contains(result.result_ref.as_ref().unwrap())
        );

        // An interrupted removal recovers idempotently without Git.
        let recovery_owner = storage
            .session_store()
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let third = svc
            .allocate_for_new_session(
                repo.path(),
                &SessionCreationOptions::isolated_worktree(BaselineMode::Commit),
            )
            .await
            .unwrap()
            .expect("isolation allocates");
        svc.bind_owner(&third.record.id, &recovery_owner.public_id)
            .await
            .unwrap();
        svc.registry().detach_owner(&third.record.id).await.unwrap();
        svc.manager().archive(&third.record).await.unwrap();
        // Simulate a removal interrupted after its intent was persisted and
        // the checkout had already been deleted: recovery completes it
        // idempotently (the vanished checkout is not recreated).
        std::fs::remove_dir_all(&third.record.checkout_path).unwrap();
        svc.registry()
            .transition_lifecycle(
                &third.record.id,
                WorkspaceLifecycle::Archived,
                WorkspaceLifecycle::Removing,
            )
            .await
            .unwrap();
        let third = svc
            .registry()
            .get_workspace(&third.record.id)
            .await
            .unwrap()
            .unwrap();
        let finished = svc.manager().remove(&third).await.unwrap();
        // The checkout had already vanished; recovery only pruned the
        // remaining Git administration, so it reports no on-disk deletion.
        assert!(!finished.checkout_removed);
        let done = svc
            .registry()
            .get_workspace(&third.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(done.lifecycle, WorkspaceLifecycle::Removed);

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
            Arc::new(GixRepoOps::new()),
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

    /// A parent session whose cwd is a *subdirectory* of its checkout (the
    /// common case for monorepo sessions) must be able to delegate: the
    /// baseline capture, repository resolution and relative-cwd mapping all
    /// run from the subdirectory, not the checkout root.
    #[tokio::test]
    async fn delegation_from_a_subdirectory_session_works() {
        let (repo, _storage, svc) = service().await;
        commit_files(
            repo.path(),
            "add sub",
            &[("file.txt", "one"), ("sub/file.txt", "sub")],
        );
        materialize(repo.path(), &[("file.txt", "one"), ("sub/file.txt", "sub")]);

        let parent = provision_owned(&svc, repo.path(), "sess-owner").await;
        let parent_sub = parent.checkout_path.join("sub");
        assert!(parent_sub.is_dir(), "fixture: parent checkout has sub/");
        fs::write(parent_sub.join("file.txt"), "parent sub edit").unwrap();
        fs::write(parent_sub.join("new.txt"), "untracked in sub").unwrap();
        fs::write(parent.checkout_path.join("root.txt"), "untracked at root").unwrap();

        let child = svc
            .allocate_for_delegation(
                "sess-owner",
                Some(&parent),
                &parent_sub,
                None,
                "delegation-sub",
                None,
            )
            .await
            .expect("delegation from a subdirectory cwd must succeed");

        // The delegate runs in the same relative subdirectory of its own
        // checkout, never inside the parent's.
        assert_ne!(child.record.checkout_path, parent.checkout_path);
        assert_eq!(
            child.effective_cwd,
            child.record.checkout_path.join("sub"),
            "delegate cwd preserves the parent's relative subdirectory"
        );
        assert!(child.effective_cwd.is_dir());

        // The baseline reflects the whole parent working state, not just
        // the subdirectory.
        assert_eq!(
            fs::read_to_string(child.effective_cwd.join("file.txt")).unwrap(),
            "parent sub edit"
        );
        assert_eq!(
            fs::read_to_string(child.effective_cwd.join("new.txt")).unwrap(),
            "untracked in sub"
        );
        assert_eq!(
            fs::read_to_string(child.record.checkout_path.join("root.txt")).unwrap(),
            "untracked at root"
        );
        assert_eq!(
            child.record.ownership.parent_workspace_id.as_ref(),
            Some(&parent.id)
        );

        // Batch baseline capture from the subdirectory resolves the same
        // repository state as from the root.
        let from_sub = svc
            .capture_batch_baseline(&parent_sub, BaselineMode::CurrentWorkingTree, None)
            .await
            .expect("baseline capture from a subdirectory");
        let from_root = svc
            .capture_batch_baseline(
                &parent.checkout_path,
                BaselineMode::CurrentWorkingTree,
                None,
            )
            .await
            .expect("baseline capture from the root");
        // Captured commits embed a timestamp, so two captures taken in
        // different seconds have different commit ids. The captured
        // *content* (tree) and the source HEAD are what must agree.
        let tree_of = |commit: &str| {
            let repo = gix::open(&parent.repository.common_dir).unwrap();
            let id = gix::ObjectId::from_hex(commit.as_bytes()).unwrap();
            repo.find_commit(id).unwrap().tree_id().unwrap().detach()
        };
        assert_eq!(
            tree_of(&from_sub.baseline_commit),
            tree_of(&from_root.baseline_commit),
            "a subdirectory cwd captures the same repository-wide content as the root"
        );
        assert_eq!(from_sub.source_head, from_root.source_head);
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
        use crate::worktree::GitRunner as _;
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
            .publish_result(
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

        // Baseline-relative review of the immutable result reports only the
        // delegate's own delta — the inherited parent edit to file.txt is
        // part of the baseline and is not misattributed to the child.
        let review = svc.review_result(&child.record, &provenance).await.unwrap();
        assert_eq!(review.baseline_commit, baseline_commit);
        assert_eq!(review.result_commit, result_commit);
        assert_eq!(review.result_ref.as_deref(), Some(result_ref));
        assert_eq!(
            review.changed_paths,
            vec![
                crate::worktree::ChangedPath {
                    path: "notes.txt".into(),
                    kind: crate::worktree::ChangeKind::Modified,
                },
                crate::worktree::ChangedPath {
                    path: "out.txt".into(),
                    kind: crate::worktree::ChangeKind::Added,
                },
            ]
        );
        assert_eq!(
            review.verification,
            Some(ResultVerificationStatus::Passed),
            "verification state is reported with the review"
        );
        assert_eq!(
            &review.captured_head,
            child
                .record
                .baseline
                .as_ref()
                .map(|b| &b.baseline_commit)
                .expect("delegated workspace has a baseline"),
            "captured HEAD is the checkout HEAD at capture time"
        );
        assert!(
            review.baseline_includes_uncommitted_edits,
            "the parent edit makes this a dirty-baseline result"
        );
        let review_text = review.summary_text();
        assert!(
            review_text.contains("notes.txt (modified)"),
            "{review_text}"
        );
        assert!(review_text.contains("out.txt (added)"), "{review_text}");
        assert!(!review_text.contains("file.txt"), "{review_text}");
        assert!(review_text.contains("WARNING"), "{review_text}");

        fs::write(child.effective_cwd.join("out.txt"), "second result").unwrap();
        let second = svc
            .publish_result(
                &child.record,
                &child.effective_cwd,
                Some(&child.mutation_gate),
                Some(ResultVerificationStatus::Skipped),
            )
            .await
            .unwrap();
        assert_ne!(second.result_ref.as_deref(), Some(result_ref));
        assert_eq!(
            svc.review_result(&child.record, &provenance).await.unwrap(),
            review,
            "a later publication never changes the earlier immutable review"
        );
        let second_review = svc.review_result(&child.record, &second).await.unwrap();
        assert_eq!(second_review.result_ref, second.result_ref);
        assert_eq!(
            second_review.verification,
            Some(ResultVerificationStatus::Skipped)
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

    /// Section 17.1/17.2: an explicit top-level publication uses the same
    /// service as delegates and reviews only the workspace's own
    /// baseline-relative delta, leaving its index and files untouched.
    #[tokio::test]
    async fn top_level_publication_reviews_only_its_own_delta() {
        use crate::worktree::{ChangeKind, ChangedPath, ResultVerificationStatus};

        let (repo, _storage, svc) = service().await;
        let record = provision_owned(&svc, repo.path(), "sess-owner").await;
        let baseline = record.baseline.as_ref().unwrap().baseline_commit.clone();

        // A modified tracked file plus a new file (never staged).
        fs::write(record.checkout_path.join("file.txt"), "top-level edit").unwrap();
        fs::write(record.checkout_path.join("added.txt"), "new").unwrap();

        let provenance = svc
            .publish_result(
                &record,
                &record.checkout_path,
                None,
                Some(ResultVerificationStatus::Skipped),
            )
            .await
            .unwrap();
        let review = svc.review_result(&record, &provenance).await.unwrap();

        assert_eq!(review.baseline_commit, baseline);
        assert_eq!(
            review.changed_paths,
            vec![
                ChangedPath {
                    path: "added.txt".into(),
                    kind: ChangeKind::Added,
                },
                ChangedPath {
                    path: "file.txt".into(),
                    kind: ChangeKind::Modified,
                },
            ]
        );
        assert_eq!(
            review.verification,
            Some(ResultVerificationStatus::Skipped),
            "explicit publication performs no verification"
        );
        assert_eq!(review.captured_head, baseline);
        // Commit-mode baseline: no inherited uncommitted edits.
        assert!(!review.baseline_includes_uncommitted_edits);
        assert!(!review.summary_text().contains("WARNING"));

        // Publication does not modify the checked-out files.
        assert_eq!(
            fs::read_to_string(record.checkout_path.join("file.txt")).unwrap(),
            "top-level edit"
        );
        assert!(record.checkout_path.join("added.txt").exists());
    }

    /// Section 17.2: review reports the captured HEAD separately from live
    /// state, so moving HEAD afterwards never rewrites the review.
    #[tokio::test]
    async fn review_reports_captured_head_separately_from_live_head() {
        let (repo, _storage, svc) = service().await;
        let record = provision_owned(&svc, repo.path(), "sess-owner").await;
        let baseline = record.baseline.as_ref().unwrap().baseline_commit.clone();

        fs::write(record.checkout_path.join("file.txt"), "edit").unwrap();
        let provenance = svc
            .publish_result(&record, &record.checkout_path, None, None)
            .await
            .unwrap();
        let review = svc.review_result(&record, &provenance).await.unwrap();
        assert_eq!(review.captured_head, baseline);

        // Move the managed branch to a new commit and change the working
        // file afterwards.
        commit_files(&record.checkout_path, "later commit", &[("other.txt", "x")]);
        fs::write(record.checkout_path.join("file.txt"), "later edit").unwrap();

        let again = svc.review_result(&record, &provenance).await.unwrap();
        assert_eq!(again, review, "later edits never rewrite the review");
        assert_eq!(again.captured_head, baseline);
    }

    /// Section 17.3: review survives restart (fresh service over the same
    /// durable registry/objects) and inspection never republishes.
    #[tokio::test]
    async fn review_is_available_after_restart_without_republishing() {
        let (repo, _storage, mut svc) = service().await;
        let record = provision_owned(&svc, repo.path(), "sess-owner").await;
        fs::write(record.checkout_path.join("out.txt"), "work").unwrap();
        let provenance = svc
            .publish_result(&record, &record.checkout_path, None, None)
            .await
            .unwrap();
        let before = svc.review_result(&record, &provenance).await.unwrap();

        // Simulate a restart: a brand-new service over the same event log,
        // with a different storage root (gates/paths are not consulted by
        // review) and no in-memory state from the first instance.
        svc = WorkspaceService::new(
            Arc::new(GixRepoOps::new()),
            svc.registry().clone(),
            TempDir::new().unwrap().path(),
        );
        let after = svc.review_result(&record, &provenance).await.unwrap();
        assert_eq!(after, before);
        assert_eq!(after.changed_paths.len(), 1);
        assert_eq!(after.changed_paths[0].path, "out.txt");

        // Inspection itself never publishes: whatever the checkout looks like
        // now, the result list is unchanged by reading it.
        fs::write(record.checkout_path.join("more.txt"), "more").unwrap();
        let listed = svc.registry().list_results(&record.id).await.unwrap();
        assert_eq!(listed.len(), 1);
        let _ = svc.review_latest_result(&record).await.unwrap();
        assert_eq!(
            svc.registry().list_results(&record.id).await.unwrap().len(),
            1,
            "review never publishes live state"
        );
    }

    /// Section 17.1: a failed publication reports an explicit error, records
    /// nothing, and leaves the checkout and its files untouched.
    #[tokio::test]
    async fn failed_publication_preserves_checkout_and_records_nothing() {
        use crate::worktree::{FailurePoint, FaultInjectingRepoOps, GixRepoOps};

        let (repo, storage, _svc) = service().await;
        let faults = std::sync::Arc::new(FaultInjectingRepoOps::new(std::sync::Arc::new(
            GixRepoOps::new(),
        )));
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        schema::init_schema(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO sessions (public_id, name, created_at, updated_at) \
             VALUES ('sess-owner', 'owner', '2026-01-15T00:00:00Z', '2026-01-15T00:00:00Z')",
            [],
        )
        .unwrap();
        let svc = WorkspaceService::new(
            faults.clone(),
            Arc::new(SqliteWorkspaceRepository::new(Arc::new(StdMutex::new(
                conn,
            )))),
            storage.path(),
        );
        let record = provision_owned(&svc, repo.path(), "sess-owner").await;
        fs::write(record.checkout_path.join("file.txt"), "kept").unwrap();

        faults.fail_next(FailurePoint::CaptureWorkingState, 1, "disk unavailable");
        let err = svc
            .publish_result(&record, &record.checkout_path, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("disk unavailable"), "{err}");
        assert!(
            svc.registry()
                .list_results(&record.id)
                .await
                .unwrap()
                .is_empty(),
            "no result is recorded by a failed publication"
        );
        assert_eq!(
            fs::read_to_string(record.checkout_path.join("file.txt")).unwrap(),
            "kept",
            "the checkout is preserved for recovery"
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

#[cfg(test)]
mod settings_tests {
    use super::*;
    use crate::config::WorkspaceSettings;
    use crate::session::sqlite_storage::SqliteStorage;

    async fn storage() -> (tempfile::TempDir, Arc<SqliteStorage>) {
        let dir = tempfile::TempDir::new().unwrap();
        let storage = SqliteStorage::connect(dir.path().join("s.db"))
            .await
            .unwrap();
        (dir, Arc::new(storage))
    }

    #[tokio::test]
    async fn disabled_settings_build_no_service() {
        let (_dir, storage) = storage().await;
        let built =
            WorkspaceService::from_settings(&WorkspaceSettings::default(), storage.as_ref());
        assert!(built.unwrap().is_none());
    }

    #[tokio::test]
    async fn enabled_settings_create_the_storage_root() {
        let (dir, storage) = storage().await;
        let root = dir.path().join("nested").join("wt");
        let settings = WorkspaceSettings {
            storage_root: Some(root.clone()),
            ..WorkspaceSettings::enabled()
        };
        let service = WorkspaceService::from_settings(&settings, storage.as_ref())
            .unwrap()
            .expect("enabled");
        assert!(root.is_dir());
        assert_eq!(service.storage_root(), root);
    }

    #[tokio::test]
    async fn unusable_storage_root_fails_at_build_time() {
        let (dir, storage) = storage().await;
        let file = dir.path().join("occupied");
        std::fs::write(&file, b"x").unwrap();
        let settings = WorkspaceSettings {
            storage_root: Some(file.join("wt")),
            ..WorkspaceSettings::enabled()
        };
        let err = WorkspaceService::from_settings(&settings, storage.as_ref())
            .err()
            .expect("must fail");
        assert!(err.to_string().contains("unusable"), "{err}");
    }

    #[tokio::test]
    async fn backend_without_a_workspace_repository_fails_at_build_time() {
        struct NoRepo(Arc<SqliteStorage>);
        impl crate::session::backend::StorageBackend for NoRepo {
            fn session_store(&self) -> Arc<dyn crate::session::store::SessionStore> {
                self.0.session_store()
            }
            fn event_journal(&self) -> Arc<dyn crate::session::projection::EventJournal> {
                self.0.event_journal()
            }
            fn view_store(&self) -> Option<Arc<dyn crate::session::projection::ViewStore>> {
                self.0.view_store()
            }
        }
        let (_dir, storage) = storage().await;
        let err = WorkspaceService::from_settings(&WorkspaceSettings::enabled(), &NoRepo(storage))
            .err()
            .expect("must fail");
        assert!(err.to_string().contains("workspace repository"), "{err}");
    }
}
