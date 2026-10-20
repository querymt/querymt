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
    Baseline, BaselineMode, ExecutionMode, ManagedBranch, ProvisioningPhase, WorkspaceId,
    WorkspaceLifecycle, WorkspaceOwnership, WorkspaceRecord, baseline_ref_for,
};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::gate::CheckoutMutationGate;
use crate::worktree::identity::{ResolvedRepository, ensure_relative_cwd_exists};
use crate::worktree::repo_ops::{RepositoryOps, validate_ref_name};
use crate::worktree::repo_workspace::WorkspaceRepository;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Where the baseline of a recovered workspace came from.
///
/// Recovery must disclose whether the baseline was durably bound before the
/// interruption (preserved exactly) or captured fresh at recovery time
/// (reflects recovery-time source contents, not the pre-interruption
/// working state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineSource {
    /// The workspace already had a bound baseline before the interruption;
    /// recovery preserved that exact identity.
    BoundBeforeInterruption,
    /// The workspace had no bound baseline; recovery captured a new one
    /// from the recorded source and policy at recovery time.
    CapturedDuringRecovery,
}

/// The result of an explicit provisioning recovery.
#[derive(Debug, Clone)]
pub struct RecoveredWorkspace {
    /// The refreshed registry record (lifecycle `Ready` on success).
    pub record: WorkspaceRecord,
    /// The baseline the recovered workspace now branches from.
    pub baseline: Baseline,
    /// Whether that baseline predates the interruption or was captured
    /// during recovery.
    pub baseline_source: BaselineSource,
}

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

        // 1. Persist the replayable provisioning intent before any Git
        //    mutation: source identity, requested ref/policy, relative cwd,
        //    intended resources, operation id, and the initial phase. The
        //    baseline is deliberately absent — capture has not run yet.
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
            requested_baseline_mode: baseline_mode,
            requested_base_ref: base_ref.map(str::to_string),
            operation_id: uuid::Uuid::new_v4().to_string(),
            provisioning_phase: Some(ProvisioningPhase::IntentPersisted),
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
        // Administration happens through the canonical common directory —
        // never through a working directory reconstructed from a checkout —
        // so separate Git directories and bare-backed linked sources
        // administer the correct shared repository. The checkout handle is
        // used only for the per-checkout capture below.
        let common_dir = &source.identity.common_dir;

        // 1. Resolve + record the baseline on the intent so recovery knows
        //    the expected branch tip. A preset batch baseline is reused
        //    verbatim (siblings share one capture); Commit mode branches
        //    from the resolved commit; current-working-tree mode captures
        //    the checkout's actual working state first (under the session's
        //    mutation gate when provided) and branches from that private
        //    baseline. Binding is immutable: a durably bound baseline is
        //    never replaced (see `recover`).
        validate_ref_name(branch.as_str())?;
        let baseline = match preset_baseline {
            Some(preset) => preset.clone(),
            None => match baseline_mode {
                BaselineMode::Commit => {
                    let base_commit =
                        resolve_base_commit(self.git.as_ref(), common_dir, base_ref).await?;
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
        self.record_phase(id, ProvisioningPhase::BaselineBound)
            .await;

        // 2. Keep the baseline reachable across restarts/GC through a
        //    managed ref. Publication is create-only and verified against
        //    the bound baseline: an existing conflicting ref is diagnosed,
        //    never overwritten (see `ensure_baseline_ref`).
        self.ensure_baseline_ref(common_dir, id, &baseline).await?;
        self.record_phase(id, ProvisioningPhase::BaselineRefPublished)
            .await;

        // 3. Allocate the managed branch at the baseline commit.
        self.ensure_branch(common_dir, branch, &baseline.baseline_commit)
            .await?;
        self.record_phase(id, ProvisioningPhase::BranchCreated)
            .await;

        // 4. Create the linked worktree checking out the managed branch.
        self.ensure_checkout(common_dir, checkout_path, branch)
            .await?;
        self.record_phase(id, ProvisioningPhase::CheckoutCreated)
            .await;

        // 5. The checkout root must exist and contain the session cwd.
        if !checkout_path.is_dir() {
            return Err(WorktreeError::InvalidOperation(format!(
                "worktree checkout missing after add: {}",
                checkout_path.display()
            )));
        }
        ensure_relative_cwd_exists(checkout_path, relative_cwd)
    }

    /// Idempotently ensure the retained baseline ref exists at the bound
    /// baseline's target. Create-only: an existing ref pointing elsewhere is
    /// refused with an explicit conflict instead of being overwritten, and
    /// an identical target is accepted so recovery can replay this step.
    async fn ensure_baseline_ref(
        &self,
        common_dir: &Path,
        id: &WorkspaceId,
        baseline: &Baseline,
    ) -> WorktreeResult<()> {
        let ref_name = baseline_ref_for(id);
        match self
            .git
            .resolve_commit(common_dir, ref_name.as_str())
            .await?
        {
            Some(target) if target == baseline.baseline_commit => Ok(()),
            Some(target) => Err(WorktreeError::InvalidOperation(format!(
                "baseline ref {ref_name} already exists at {target}; refusing to \
                 republish the bound baseline {} — resolve the conflict explicitly",
                baseline.baseline_commit
            ))),
            None => {
                self.git
                    .create_ref_if_absent(common_dir, ref_name.as_str(), &baseline.baseline_commit)
                    .await
            }
        }
    }

    /// Persist a forward-only provisioning phase. Best-effort: a failed
    /// phase write never aborts allocation (the phase is advisory recovery
    /// state; resource idempotence and the lifecycle CAS remain the
    /// authoritative boundaries).
    async fn record_phase(&self, id: &WorkspaceId, phase: ProvisioningPhase) {
        let _ = self.registry.record_provisioning_phase(id, phase).await;
    }

    /// Create the managed branch at `expected_base` unless it already
    /// exists there. A branch pointing elsewhere is a hard conflict.
    async fn ensure_branch(
        &self,
        common_dir: &Path,
        branch: &ManagedBranch,
        expected_base: &str,
    ) -> WorktreeResult<()> {
        if let Some(tip) = self.git.branch_tip(common_dir, branch).await? {
            if tip == expected_base {
                return Ok(());
            }
            return Err(WorktreeError::InvalidOperation(format!(
                "managed branch {} exists at {} but the recorded base is {}",
                branch, tip, expected_base
            )));
        }
        self.git
            .create_branch(common_dir, branch, expected_base)
            .await
    }

    /// Create the linked checkout unless the path is already a registered
    /// worktree of `branch`. A pre-existing directory that Git does not
    /// know as this worktree is never touched — it is preserved and the
    /// operation fails loudly.
    async fn ensure_checkout(
        &self,
        common_dir: &Path,
        checkout_path: &Path,
        branch: &ManagedBranch,
    ) -> WorktreeResult<()> {
        if checkout_path.exists() {
            if worktree_registered(self.git.as_ref(), common_dir, checkout_path, branch).await? {
                return Ok(());
            }
            return Err(WorktreeError::InvalidOperation(format!(
                "preserving pre-existing unknown directory at {}: it is not a registered checkout of {}",
                checkout_path.display(),
                branch
            )));
        }

        self.git
            .add_worktree(common_dir, checkout_path, branch)
            .await
    }

    /// Idempotently finish an interrupted provisioning attempt.
    ///
    /// Replays the boundary steps ([`Self::provision`]) for a record still
    /// in `Provisioning`; safe to run after a crash at any boundary. Never
    /// touches pre-existing unknown directories at the checkout path.
    ///
    /// Recovery is self-contained: administration runs against the record's
    /// canonical common directory and recovery-time capture uses the
    /// recorded source checkout — no working directory is guessed or passed
    /// in by callers.
    pub async fn recover(&self, record: &WorkspaceRecord) -> WorktreeResult<RecoveredWorkspace> {
        if record.lifecycle != WorkspaceLifecycle::Provisioning {
            return Err(WorktreeError::InvalidOperation(format!(
                "workspace {} is {:?}, only provisioning workspaces can be recovered",
                record.id, record.lifecycle
            )));
        }

        // Whether the baseline was bound before recovery determines the
        // disclosure: a bound baseline is preserved exactly; an absent one
        // is (re)captured at recovery time and disclosed as such.
        let had_bound_baseline = record.baseline.is_some();
        let common_dir = record.repository.common_dir.clone();
        let recovery = self.recover_inner(record);
        crate::worktree::lock::with_admin_lock(&self.storage_root, &common_dir, recovery).await?;

        let updated = self
            .registry
            .get_workspace(&record.id)
            .await?
            .ok_or_else(|| {
                WorktreeError::InvalidOperation(format!(
                    "workspace {} disappeared during recovery",
                    record.id
                ))
            })?;
        let baseline = updated.baseline.clone().ok_or_else(|| {
            WorktreeError::InvalidOperation(format!(
                "workspace {} has no baseline after recovery",
                record.id
            ))
        })?;
        let baseline_source = if had_bound_baseline {
            BaselineSource::BoundBeforeInterruption
        } else {
            BaselineSource::CapturedDuringRecovery
        };
        Ok(RecoveredWorkspace {
            record: updated,
            baseline,
            baseline_source,
        })
    }

    async fn recover_inner(&self, record: &WorkspaceRecord) -> WorktreeResult<()> {
        // 1. Reconcile the baseline. A durably bound baseline is preserved
        //    exactly — never recaptured or changed. A baseline-absent intent
        //    is retried with a freshly validated capture from the recorded
        //    source and policy; on failure the workspace keeps its
        //    actionable non-runnable state for an explicit later retry.
        if record.baseline.is_some() {
            return self.finish_recovery(record).await;
        }
        let baseline = match self.capture_recovery_baseline(record).await {
            Ok(baseline) => baseline,
            Err(err) => {
                self.record_recovery_error(&record.id, &err).await;
                return Err(err);
            }
        };
        if let Err(err) = self.registry.bind_baseline(&record.id, &baseline).await {
            let err = WorktreeError::from(err);
            self.record_recovery_error(&record.id, &err).await;
            return Err(err);
        }
        self.record_phase(&record.id, ProvisioningPhase::BaselineBound)
            .await;
        self.finish_recovery_with(record, &baseline).await
    }

    /// Replay the resource boundaries after the baseline is reconciled:
    /// retained ref, branch, checkout, relative cwd, then the final Ready
    /// transition.
    async fn finish_recovery(&self, record: &WorkspaceRecord) -> WorktreeResult<()> {
        let baseline = record.baseline.clone().ok_or_else(|| {
            WorktreeError::InvalidOperation(format!(
                "provisioning intent for {} has no recorded base commit",
                record.id
            ))
        })?;
        self.finish_recovery_with(record, &baseline).await
    }

    async fn finish_recovery_with(
        &self,
        record: &WorkspaceRecord,
        baseline: &Baseline,
    ) -> WorktreeResult<()> {
        let common_dir = &record.repository.common_dir;
        self.ensure_baseline_ref(common_dir, &record.id, baseline)
            .await?;
        self.record_phase(&record.id, ProvisioningPhase::BaselineRefPublished)
            .await;
        self.ensure_branch(common_dir, &record.branch, &baseline.baseline_commit)
            .await?;
        self.record_phase(&record.id, ProvisioningPhase::BranchCreated)
            .await;
        self.ensure_checkout(common_dir, &record.checkout_path, &record.branch)
            .await?;
        self.record_phase(&record.id, ProvisioningPhase::CheckoutCreated)
            .await;
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
    }

    /// Build the recovery-time baseline from the recorded intent policy.
    /// Commit mode re-resolves the recorded ref (or HEAD) in the recorded
    /// common repository; current-working-tree mode re-runs the validated
    /// capture against the recorded source checkout. The result is
    /// disclosed as captured during recovery, never presented as the
    /// pre-interruption capture.
    async fn capture_recovery_baseline(
        &self,
        record: &WorkspaceRecord,
    ) -> WorktreeResult<Baseline> {
        match record.requested_baseline_mode {
            BaselineMode::Commit => {
                let base_commit = resolve_base_commit(
                    self.git.as_ref(),
                    &record.repository.common_dir,
                    record.requested_base_ref.as_deref(),
                )
                .await?;
                Ok(Baseline {
                    mode: BaselineMode::Commit,
                    source_head: base_commit.clone(),
                    baseline_commit: base_commit,
                })
            }
            BaselineMode::CurrentWorkingTree => {
                let captured = crate::worktree::capture::capture_working_state_validated(
                    self.git.as_ref(),
                    &record.repository.origin_path,
                    Default::default(),
                )
                .await?;
                Ok(Baseline {
                    mode: BaselineMode::CurrentWorkingTree,
                    source_head: captured.source_head,
                    baseline_commit: captured.commit,
                })
            }
        }
    }

    /// Keep a failed recovery actionable: the workspace stays non-runnable
    /// (`provisioning`, so an explicit retry remains possible) and the
    /// diagnostic is retained on the record.
    async fn record_recovery_error(&self, id: &WorkspaceId, error: &WorktreeError) {
        if let Ok(Some(mut rec)) = self.registry.get_workspace(id).await {
            rec.last_error = Some(error.to_string());
            rec.updated_at = time::OffsetDateTime::now_utc();
            let _ = self.registry.update_workspace(rec).await;
        }
    }

    /// Recover every `Provisioning` workspace of a repository. Per-record
    /// failures are skipped (they stay `Provisioning`/`Failed` for the
    /// management surface); successes are returned.
    pub async fn recover_pending(&self, common_dir: &Path) -> WorktreeResult<Vec<WorkspaceRecord>> {
        let pending: Vec<WorkspaceRecord> = self
            .registry
            .list_workspaces_by_repository(common_dir)
            .await?
            .into_iter()
            .filter(|r| r.lifecycle == WorkspaceLifecycle::Provisioning)
            .collect();

        let mut recovered = Vec::new();
        for record in pending {
            if let Ok(done) = self.recover(&record).await {
                recovered.push(done.record);
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
    common_dir: &Path,
    checkout_path: &Path,
    branch: &ManagedBranch,
) -> WorktreeResult<bool> {
    let checkout_canonical = std::fs::canonicalize(checkout_path).ok();
    for registered in ops.list_worktrees(common_dir).await? {
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

/// Resolve `base_ref` (or HEAD) to a full commit object id in the
/// canonical common repository.
async fn resolve_base_commit(
    ops: &dyn RepositoryOps,
    common_dir: &Path,
    base_ref: Option<&str>,
) -> WorktreeResult<String> {
    let expr = base_ref.unwrap_or("HEAD");
    validate_rev_expression(expr)?;
    let rev = format!("{expr}^{{commit}}");
    match ops.resolve_commit(common_dir, &rev).await? {
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
    use crate::worktree::repo_ops::{FailurePoint, FaultInjectingRepoOps};
    use crate::worktree::repo_workspace::SqliteWorkspaceRepository;
    use crate::worktree::test_support::{
        FaultInjectingWorkspaceRepository, RegistryFaultPoint, init_repo_with_subdir, materialize,
    };
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

    fn resolve(repo_path: &Path) -> ResolvedRepository {
        crate::worktree::identity::resolve_repository(repo_path).unwrap()
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
        let mut record =
            insert_intent_only(alloc, storage, resolved, id, BaselineMode::Commit, None).await;
        record.baseline = Some(Baseline {
            mode: BaselineMode::Commit,
            source_head: base.to_string(),
            baseline_commit: base.to_string(),
        });
        record.provisioning_phase = Some(crate::worktree::ProvisioningPhase::BaselineBound);
        alloc
            .registry
            .bind_baseline(&record.id, record.baseline.as_ref().unwrap())
            .await
            .unwrap();
        record
    }

    /// Persist only the provisioning intent — the true first durable state:
    /// source identity, requested policy, placement, and phase, with no
    /// baseline. This is the shape a crash directly after intent persistence
    /// leaves behind.
    async fn insert_intent_only(
        alloc: &WorkspaceAllocator,
        storage: &Path,
        resolved: &ResolvedRepository,
        id: &str,
        baseline_mode: BaselineMode,
        base_ref: Option<&str>,
    ) -> WorkspaceRecord {
        let wid = WorkspaceId::from_raw(id);
        let record = WorkspaceRecord {
            id: wid.clone(),
            repository: resolved.identity.clone(),
            checkout_path: checkout_path_for(storage, &resolved.identity.common_dir, &wid),
            branch: ManagedBranch::for_workspace(&wid),
            relative_cwd: PathBuf::new(),
            execution: ExecutionMode::Worktree,
            baseline: None,
            lifecycle: WorkspaceLifecycle::Provisioning,
            ownership: WorkspaceOwnership::default(),
            last_error: None,
            requested_baseline_mode: baseline_mode,
            requested_base_ref: base_ref.map(str::to_string),
            operation_id: format!("op-{id}"),
            provisioning_phase: Some(crate::worktree::ProvisioningPhase::IntentPersisted),
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

        let done = alloc.recover(&record).await.unwrap();
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
        assert!(done.record.checkout_path.join("file.txt").exists());
        assert_eq!(
            branch_tip(&alloc, repo.path(), &done.record.branch).await,
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

        let done = alloc.recover(&record).await.unwrap();
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
        // The pre-existing branch was reused, not recreated or moved.
        assert_eq!(
            branch_tip(&alloc, repo.path(), &done.record.branch).await,
            head.to_string()
        );
        assert!(done.record.checkout_path.join("file.txt").exists());
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

        let done = alloc.recover(&record).await.unwrap();
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
        assert!(done.record.checkout_path.join("file.txt").exists());
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

        let err = alloc.recover(&record).await.unwrap_err();
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

        let err = alloc.recover(&record).await.unwrap_err();
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
            .recover_pending(&resolved.identity.common_dir)
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

    // -- Replayable provisioning and retained baseline refs (section 15) ----

    /// In-memory registry store, for fault-injected allocators.
    fn in_memory_registry() -> Arc<SqliteWorkspaceRepository> {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        schema::init_schema(&mut conn).unwrap();
        Arc::new(SqliteWorkspaceRepository::new(Arc::new(Mutex::new(conn))))
    }

    /// Allocator whose registry can be faulted at persistence boundaries.
    fn faulted_registry(
        storage: &Path,
    ) -> (WorkspaceAllocator, Arc<FaultInjectingWorkspaceRepository>) {
        let inner = in_memory_registry();
        let faulted = Arc::new(FaultInjectingWorkspaceRepository::new(inner));
        let alloc = WorkspaceAllocator::new(
            Arc::new(GixRepoOps::new()),
            faulted.clone() as Arc<dyn WorkspaceRepository>,
            storage.to_path_buf(),
        );
        (alloc, faulted)
    }

    /// Allocator over a file-backed registry, for restart simulations.
    fn file_allocator(
        storage: &Path,
        db_path: &Path,
    ) -> (WorkspaceAllocator, Arc<Mutex<Connection>>) {
        let mut conn = Connection::open(db_path).unwrap();
        conn.execute("PRAGMA foreign_keys = ON;", []).unwrap();
        schema::init_schema(&mut conn).unwrap();
        let conn = Arc::new(Mutex::new(conn));
        let alloc = WorkspaceAllocator::new(
            Arc::new(GixRepoOps::new()),
            Arc::new(SqliteWorkspaceRepository::new(conn.clone())),
            storage.to_path_buf(),
        );
        (alloc, conn)
    }

    /// Insert the session rows required by the owner foreign key.
    async fn insert_session_row(conn: &Arc<Mutex<Connection>>, public_id: &str) {
        let conn = conn.clone();
        let public_id = public_id.to_string();
        tokio::task::spawn_blocking(move || {
            conn.lock()
                .unwrap()
                .execute(
                    "INSERT INTO sessions (public_id, name, created_at, updated_at) \
                     VALUES (?1, 'test', '2026-01-15T00:00:00Z', '2026-01-15T00:00:00Z')",
                    [&public_id],
                )
                .unwrap();
        })
        .await
        .unwrap();
    }

    /// Current target of a ref, or `None` when it does not exist.
    async fn ref_target(alloc: &WorkspaceAllocator, repo: &Path, name: &str) -> Option<String> {
        alloc.git().resolve_commit(repo, name).await.unwrap()
    }

    /// The true first persisted intent round-trips without a fabricated
    /// baseline and with no Git resources allocated yet.
    #[tokio::test]
    async fn first_persisted_intent_round_trips_without_a_fabricated_baseline() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let intent = insert_intent_only(
            &alloc,
            storage.path(),
            &resolved,
            "ws-first",
            BaselineMode::CurrentWorkingTree,
            None,
        )
        .await;

        let stored = alloc
            .registry
            .get_workspace(&intent.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            stored.baseline.is_none(),
            "the first intent must not fabricate a baseline"
        );
        assert_eq!(
            stored.requested_baseline_mode,
            BaselineMode::CurrentWorkingTree
        );
        assert_eq!(stored.requested_base_ref, None);
        assert!(!stored.operation_id.is_empty(), "operation id persisted");
        assert_eq!(
            stored.provisioning_phase,
            Some(crate::worktree::ProvisioningPhase::IntentPersisted)
        );

        // Nothing was allocated for the intent: no checkout, no branch, no
        // retained baseline ref.
        assert!(!intent.checkout_path.exists());
        assert_eq!(
            ref_target(
                &alloc,
                repo.path(),
                &format!("refs/heads/{}", intent.branch.as_str())
            )
            .await,
            None
        );
        assert_eq!(
            ref_target(&alloc, repo.path(), &baseline_ref_for(&intent.id)).await,
            None
        );
    }

    /// Baseline-absent recovery works from the durable intent alone, across
    /// a restart, and discloses the recovery-time capture instead of
    /// presenting it as the pre-interruption state.
    #[tokio::test]
    async fn recover_baseline_absent_intent_captures_at_recovery_time_and_discloses() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let db = TempDir::new().unwrap();

        // Persist the intent through a first "process"; recover through a
        // reopened registry to prove the durable intent alone suffices.
        let record = {
            let (alloc, conn) = file_allocator(storage.path(), &db.path().join("state.db"));
            let resolved = resolve(repo.path());
            let record = insert_intent_only(
                &alloc,
                storage.path(),
                &resolved,
                "ws-recover",
                BaselineMode::CurrentWorkingTree,
                None,
            )
            .await;
            (record, conn)
        };
        let (record, conn) = record;

        // The source changed after the interruption.
        std::fs::write(repo.path().join("file.txt"), "recovered").unwrap();

        let store = Arc::new(SqliteWorkspaceRepository::new(conn.clone()));
        let persisted = store
            .get_workspace(&record.id)
            .await
            .unwrap()
            .expect("intent survives the restart");
        assert!(persisted.baseline.is_none());

        let alloc =
            WorkspaceAllocator::new(Arc::new(GixRepoOps::new()), store.clone(), storage.path());
        let done = alloc.recover(&persisted).await.unwrap();
        assert_eq!(
            done.baseline_source,
            BaselineSource::CapturedDuringRecovery,
            "the recovery-time capture is disclosed"
        );
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(done.baseline.mode, BaselineMode::CurrentWorkingTree);
        assert_eq!(
            fs::read_to_string(done.record.checkout_path.join("file.txt")).unwrap(),
            "recovered",
            "worktree reflects recovery-time source contents"
        );
        assert_eq!(
            branch_tip(&alloc, repo.path(), &done.record.branch).await,
            done.baseline.baseline_commit
        );
        assert_eq!(
            ref_target(&alloc, repo.path(), &baseline_ref_for(&done.record.id))
                .await
                .as_deref(),
            Some(done.baseline.baseline_commit.as_str()),
            "the retained ref exists at the recovery-time baseline"
        );
        // No duplicate resources: exactly one workspace row exists.
        assert_eq!(alloc.registry.list_workspaces().await.unwrap().len(), 1);
        // The source checkout was not disturbed by recovery.
        assert_eq!(
            fs::read_to_string(repo.path().join("file.txt")).unwrap(),
            "recovered"
        );
    }

    /// Commit-mode baseline-absent recovery re-resolves the recorded
    /// requested ref instead of fabricating a baseline.
    #[tokio::test]
    async fn recover_baseline_absent_commit_intent_re_resolves_the_requested_ref() {
        let (repo, first, _second) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent_only(
            &alloc,
            storage.path(),
            &resolved,
            "ws-commit",
            BaselineMode::Commit,
            Some(&first.to_string()),
        )
        .await;

        let done = alloc.recover(&record).await.unwrap();
        assert_eq!(done.baseline_source, BaselineSource::CapturedDuringRecovery);
        assert_eq!(done.baseline.mode, BaselineMode::Commit);
        assert_eq!(done.baseline.baseline_commit, first.to_string());
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(
            branch_tip(&alloc, repo.path(), &done.record.branch).await,
            first.to_string()
        );
    }

    /// An unavailable source leaves an actionable non-runnable record; a
    /// later explicit retry succeeds without duplicate resources.
    #[tokio::test]
    async fn recover_baseline_absent_with_unavailable_source_stays_actionable_and_retryable() {
        let (repo, first, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent_only(
            &alloc,
            storage.path(),
            &resolved,
            "ws-gone",
            BaselineMode::Commit,
            Some("refs/heads/vanished"),
        )
        .await;

        // The recorded ref no longer resolves: recovery fails with an
        // actionable diagnostic and keeps the workspace non-runnable.
        let err = alloc.recover(&record).await.unwrap_err();
        assert!(err.to_string().contains("refs/heads/vanished"), "{err}");
        let stored = alloc
            .registry
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Provisioning);
        assert!(
            stored
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("refs/heads/vanished"),
            "diagnostic is actionable: {:?}",
            stored.last_error
        );
        assert!(!stored.checkout_path.exists());

        // Once the source is available again, the same intent recovers.
        CliGit::new()
            .run(repo.path(), &["branch", "vanished", &first.to_string()])
            .await
            .unwrap()
            .into_checked(&["branch"])
            .unwrap();
        let done = alloc.recover(&stored).await.unwrap();
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(done.baseline.baseline_commit, first.to_string());
        assert_eq!(alloc.registry.list_workspaces().await.unwrap().len(), 1);
    }

    /// A crash between baseline binding and ref publication is repaired:
    /// recovery restores that exact baseline into the retained ref.
    #[tokio::test]
    async fn crash_after_baseline_binding_restores_the_exact_baseline_ref() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        // insert_intent binds the baseline durably but publishes no ref and
        // allocates nothing: the crash boundary between binding and
        // publication.
        let record = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-ref",
            &head.to_string(),
        )
        .await;
        let bound = record.baseline.clone().unwrap();
        assert_eq!(
            ref_target(&alloc, repo.path(), &baseline_ref_for(&record.id)).await,
            None,
            "precondition: the ref was never published"
        );

        let done = alloc.recover(&record).await.unwrap();
        assert_eq!(
            done.baseline_source,
            BaselineSource::BoundBeforeInterruption
        );
        assert_eq!(
            done.baseline, bound,
            "the bound baseline identity is preserved exactly"
        );
        assert_eq!(
            ref_target(&alloc, repo.path(), &baseline_ref_for(&record.id))
                .await
                .as_deref(),
            Some(bound.baseline_commit.as_str()),
            "the exact bound baseline was restored into the retained ref"
        );
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
    }

    /// A conflicting existing baseline ref is diagnosed rather than
    /// overwritten, and the workspace stays recoverable.
    #[tokio::test]
    async fn conflicting_existing_baseline_ref_is_refused_not_overwritten() {
        let (repo, first, second) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let alloc = allocator(storage.path());
        let resolved = resolve(repo.path());

        let record = insert_intent(
            &alloc,
            storage.path(),
            &resolved,
            "ws-conflict",
            &first.to_string(),
        )
        .await;
        let ref_name = baseline_ref_for(&record.id);
        // An external writer created the retained ref at a different commit.
        CliGit::new()
            .run(
                repo.path(),
                &["update-ref", ref_name.as_str(), &second.to_string()],
            )
            .await
            .unwrap()
            .into_checked(&["update-ref"])
            .unwrap();

        let err = alloc.recover(&record).await.unwrap_err();
        assert!(
            matches!(err, WorktreeError::InvalidOperation(ref m) if m.contains("refusing")),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            ref_target(&alloc, repo.path(), &ref_name).await.as_deref(),
            Some(second.to_string().as_str()),
            "the conflicting ref is not overwritten"
        );
        let stored = alloc
            .registry
            .get_workspace(&record.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.lifecycle,
            WorkspaceLifecycle::Provisioning,
            "the workspace stays recoverable after the conflict is resolved"
        );
        assert!(
            !stored.checkout_path.exists(),
            "no checkout is created while the ref conflicts"
        );
    }

    /// Git-operation fault injection at the capture, baseline-ref
    /// publication, branch, and checkout boundaries: failed allocations
    /// mark the record failed, keep resources consistent, and recovery
    /// completes without duplicates.
    #[tokio::test]
    async fn git_boundary_faults_fail_allocations_without_orphaning_resources() {
        let (repo, _, _) = init_repo_with_subdir();
        let resolved = resolve(repo.path());

        // (a) capture boundary: nothing bound, no branch, no checkout.
        let storage = TempDir::new().unwrap();
        let faulted = Arc::new(FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new())));
        let alloc = WorkspaceAllocator::new(
            faulted.clone() as Arc<dyn RepositoryOps>,
            in_memory_registry(),
            storage.path(),
        );
        faulted.fail_next(FailurePoint::CaptureWorkingState, 1, "capture boom");
        let record = alloc
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
            .unwrap_err();
        assert!(record.to_string().contains("capture boom"));
        let stored = in_memory_registry_failed_row(&alloc).await;
        assert!(!stored.checkout_path.exists());

        // (b) baseline-ref publication boundary: baseline bound, but no
        //     branch/checkout allocated.
        let storage = TempDir::new().unwrap();
        let faulted = Arc::new(FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new())));
        let alloc = WorkspaceAllocator::new(
            faulted.clone() as Arc<dyn RepositoryOps>,
            in_memory_registry(),
            storage.path(),
        );
        faulted.fail_next(FailurePoint::CreateRefIfAbsent, 1, "ref boom");
        let err = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ref boom"));
        let stored = in_memory_registry_failed_row(&alloc).await;
        assert!(!stored.checkout_path.exists());
        assert_eq!(
            ref_target(
                &alloc,
                repo.path(),
                &format!("refs/heads/{}", stored.branch.as_str())
            )
            .await,
            None,
            "no branch is allocated when ref publication fails"
        );

        // (c) branch creation boundary.
        let storage = TempDir::new().unwrap();
        let faulted = Arc::new(FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new())));
        let alloc = WorkspaceAllocator::new(
            faulted.clone() as Arc<dyn RepositoryOps>,
            in_memory_registry(),
            storage.path(),
        );
        faulted.fail_next(FailurePoint::CreateBranch, 1, "branch boom");
        let err = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("branch boom"));
        let stored = in_memory_registry_failed_row(&alloc).await;
        assert!(!stored.checkout_path.exists());

        // (d) checkout creation boundary: the branch already exists, and
        //     explicit recovery completes it without duplicate resources.
        let storage = TempDir::new().unwrap();
        let faulted = Arc::new(FaultInjectingRepoOps::new(Arc::new(GixRepoOps::new())));
        let alloc = WorkspaceAllocator::new(
            faulted.clone() as Arc<dyn RepositoryOps>,
            in_memory_registry(),
            storage.path(),
        );
        faulted.fail_next(FailurePoint::AddWorktree, 1, "checkout boom");
        let err = alloc
            .allocate(&resolved, Path::new(""), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checkout boom"));
        let stored = alloc
            .registry
            .list_workspaces()
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(stored.lifecycle, WorkspaceLifecycle::Failed);
        assert!(!stored.checkout_path.exists());
        let baseline = stored.baseline.clone().unwrap();
        assert_eq!(
            ref_target(
                &alloc,
                repo.path(),
                &format!("refs/heads/{}", stored.branch.as_str())
            )
            .await
            .as_deref(),
            Some(baseline.baseline_commit.as_str()),
            "the branch exists at the baseline from before the failure"
        );

        // Simulate the durable retry path: back to provisioning, recover.
        alloc
            .registry
            .transition_lifecycle(
                &stored.id,
                WorkspaceLifecycle::Failed,
                WorkspaceLifecycle::Provisioning,
            )
            .await
            .unwrap();
        let retryable = alloc
            .registry
            .get_workspace(&stored.id)
            .await
            .unwrap()
            .unwrap();
        let done = alloc.recover(&retryable).await.unwrap();
        assert_eq!(done.baseline, baseline, "bound baseline preserved");
        assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
        assert_eq!(
            ref_target(
                &alloc,
                repo.path(),
                &format!("refs/heads/{}", stored.branch.as_str())
            )
            .await
            .as_deref(),
            Some(baseline.baseline_commit.as_str()),
            "recovery reuses the existing branch without moving it"
        );
        assert_eq!(alloc.registry.list_workspaces().await.unwrap().len(), 1);
    }

    /// Fetch the single workspace row and assert it is Failed with a
    /// retained diagnostic.
    async fn in_memory_registry_failed_row(alloc: &WorkspaceAllocator) -> WorkspaceRecord {
        let rows = alloc.registry.list_workspaces().await.unwrap();
        assert_eq!(rows.len(), 1, "exactly the failed intent is stored");
        assert_eq!(rows[0].lifecycle, WorkspaceLifecycle::Failed);
        assert!(rows[0].last_error.is_some(), "diagnostic retained");
        rows[0].clone()
    }

    /// Registry persistence faults at the intent, baseline-bind, and final
    /// Ready boundaries: nothing is silently lost and provisioning stays
    /// replayable.
    #[tokio::test]
    async fn registry_boundary_faults_keep_provisioning_replayable() {
        let (repo, _, head) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let resolved = resolve(repo.path());

        // (a) intent persistence failure: no row, no Git resources.
        {
            let (alloc, faults) = faulted_registry(storage.path());
            faults.fail_next(RegistryFaultPoint::InsertWorkspace, 1, "intent boom");
            let err = alloc
                .allocate(&resolved, Path::new(""), None)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("intent boom"));
            assert_eq!(
                alloc.registry.list_workspaces().await.unwrap().len(),
                0,
                "no intent row was persisted"
            );
            assert_eq!(
                ref_target(&alloc, repo.path(), "HEAD").await.as_deref(),
                Some(head.to_string().as_str()),
                "source HEAD untouched"
            );
        }

        // (b) baseline binding failure: the record fails closed with no
        //     Git resources allocated.
        {
            let (alloc, faults) = faulted_registry(storage.path());
            faults.fail_next(RegistryFaultPoint::BindBaseline, 1, "bind boom");
            let err = alloc
                .allocate(&resolved, Path::new(""), None)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("bind boom"));
            let rows = alloc.registry.list_workspaces().await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].lifecycle, WorkspaceLifecycle::Failed);
            assert!(rows[0].baseline.is_none());
            assert!(!rows[0].checkout_path.exists());
        }

        // (c) final Ready-transition failure: Git resources exist, the
        //     workspace stays provisioning, and recovery completes without
        //     duplicates or a second owner.
        {
            let (alloc, faults) = faulted_registry(storage.path());
            faults.fail_next(RegistryFaultPoint::TransitionLifecycle, 1, "ready boom");
            let err = alloc
                .allocate(&resolved, Path::new(""), None)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("ready boom"));
            let rows = alloc.registry.list_workspaces().await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].lifecycle, WorkspaceLifecycle::Provisioning);
            assert!(
                rows[0].checkout_path.is_dir(),
                "allocated resources are recoverable"
            );
            let done = alloc.recover(&rows[0]).await.unwrap();
            assert_eq!(done.record.lifecycle, WorkspaceLifecycle::Ready);
            assert_eq!(alloc.registry.list_workspaces().await.unwrap().len(), 1);
        }
    }

    /// Recovery preserves single ownership across a restart: exactly one
    /// workspace row, one writable owner, and a second attach is refused.
    #[tokio::test]
    async fn recovery_preserves_single_ownership_across_restart() {
        let (repo, _, _) = init_repo_with_subdir();
        let storage = TempDir::new().unwrap();
        let db = TempDir::new().unwrap();

        let record = {
            let (alloc, conn) = file_allocator(storage.path(), &db.path().join("state.db"));
            let resolved = resolve(repo.path());
            let record = insert_intent_only(
                &alloc,
                storage.path(),
                &resolved,
                "ws-owner",
                BaselineMode::CurrentWorkingTree,
                None,
            )
            .await;
            (record, conn)
        };
        let (record, conn) = record;
        insert_session_row(&conn, "sess-a").await;
        insert_session_row(&conn, "sess-b").await;

        let store = Arc::new(SqliteWorkspaceRepository::new(conn.clone()));
        let persisted = store
            .get_workspace(&record.id)
            .await
            .unwrap()
            .expect("intent survives the restart");
        let alloc =
            WorkspaceAllocator::new(Arc::new(GixRepoOps::new()), store.clone(), storage.path());
        let done = alloc.recover(&persisted).await.unwrap();

        store.attach_owner(&done.record.id, "sess-a").await.unwrap();
        let err = store
            .attach_owner(&done.record.id, "sess-b")
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                crate::session::error::SessionError::WorkspaceAlreadyOwned { .. }
            ),
            "second writable owner refused: {err:?}"
        );
        assert_eq!(store.list_workspaces().await.unwrap().len(), 1);
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
