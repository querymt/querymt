//! Deterministic integration of an accepted delegate result.
//!
//! No model is involved: given the delegate's fixed baseline `B0` and its
//! retained result commit `R`, the baseline-relative delta is applied to the
//! checkout's *current branch* as one commit.
//!
//! - A three-way merge (`B0`, `HEAD`, `R`) produces the target tree. When no
//!   earlier result was integrated `HEAD` still matches the baseline for every
//!   touched path and the merge reduces to taking `R`; when a parallel sibling
//!   was integrated first, the same merge layers this result on top of it and
//!   reports overlapping edits as a conflict instead of guessing.
//! - Only the paths that actually change are written. Any of them carrying
//!   staged, unstaged, untracked or ignored local state aborts the
//!   integration, so the user's own work is never swept into the commit.
//! - Repository hooks run natively and the current branch advances with a
//!   compare-and-swap ref update (no Git executable is involved).
//!
//! [`WorkspaceService::accept_delegation_result`] resolves everything from
//! durable state, then runs the phases (recover, prepare, apply, hooks,
//! finalize) under the checkout's mutation gate. Any failure after files were
//! applied is followed by `recover_integration`, so a rejected hook or a
//! failed phase never leaves applied files or an intent behind.
//!
//! Callers serialize integrations on a checkout with the cooperative
//! [`CheckoutMutationGate`](crate::worktree::CheckoutMutationGate).

use crate::worktree::domain::{Baseline, ChangedPath};
use crate::worktree::error::{WorktreeError, WorktreeResult};
use crate::worktree::gate::CheckoutMutationGate;
use crate::worktree::hooks::{HookOutcome, HookRequest, HookedIntegration};
use crate::worktree::identity::resolve_repository;
use crate::worktree::protect::IndexLock;
use crate::worktree::service::WorkspaceService;
use std::path::{Path, PathBuf};

/// A successfully created integration commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegratedCommit {
    pub commit: String,
    /// `HEAD` the commit was created on top of.
    pub parent: String,
    /// Short name of the branch that was advanced.
    pub branch: String,
    pub changed_paths: Vec<ChangedPath>,
}

/// A precondition failed; nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Stable machine-readable code.
    pub code: &'static str,
    pub message: String,
    /// Offending repository-relative paths, when applicable.
    pub paths: Vec<String>,
}

impl Refusal {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            paths: Vec::new(),
        }
    }

    pub(crate) fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.paths = paths;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrationOutcome {
    Integrated(IntegratedCommit),
    /// This exact result was integrated before; no second commit is made.
    AlreadyIntegrated {
        commit: String,
    },
    /// The current branch already contains the result's whole delta.
    NoChanges,
    /// Overlapping edits; the checkout is unchanged.
    Conflicted {
        paths: Vec<String>,
    },
    Refused(Refusal),
}

/// Input of
/// [`RepositoryOps::prepare_integration`](crate::worktree::RepositoryOps::prepare_integration);
/// all object ids are full commit ids resolved from durable state.
#[derive(Debug, Clone)]
pub struct PrepareRequest {
    /// Target checkout (root or any directory below it) whose current
    /// branch would receive the commit.
    pub checkout: PathBuf,
    /// The delegate's fixed baseline (`B0`).
    pub baseline_commit: String,
    /// The reviewed, immutable result (`R`).
    pub result_commit: String,
    /// HEAD the baseline was captured on. Delta paths that already differed
    /// between this commit and the baseline carry uncommitted parent edits
    /// and are refused (`baseline_local_edits`). `None` skips that check.
    pub source_head: Option<String>,
}

/// A result that passed preflight and local-work protection and merges
/// cleanly onto the checkout's current HEAD. Nothing has been written to the
/// checkout, its index contents or any ref; only the merged tree object
/// exists in the object database. The checkout's real `index.lock` is held
/// until this value is dropped or consumed by a later phase.
#[derive(Debug, PartialEq, Eq)]
pub struct PreparedIntegration {
    /// Working-tree root of the target checkout (the directory the touched
    /// paths are relative to).
    pub checkout: PathBuf,
    /// `HEAD` commit the integration commit would be created on.
    pub head: String,
    /// Short name of the branch that would advance.
    pub branch: String,
    pub head_tree: String,
    /// Result of `merge_trees(B0, HEAD, R)`.
    pub merged_tree: String,
    /// Baseline-relative delta `B0 -> R` (no rename detection).
    pub delta: Vec<ChangedPath>,
    /// Paths that actually change on this checkout, `HEAD -> merged tree`.
    /// A subset of `delta`: paths a sibling already integrated drop out.
    pub touched: Vec<ChangedPath>,
    /// Exclusive hold on the checkout's index for the whole transaction.
    pub lock: IndexLock,
}

/// Outcome of the read-only preparation phase.
#[derive(Debug, PartialEq, Eq)]
pub enum PrepareOutcome {
    Ready(PreparedIntegration),
    /// The current branch already contains the result's whole delta.
    NoChanges,
    /// Overlapping edits; nothing was written.
    Conflicted {
        paths: Vec<String>,
    },
    Refused(Refusal),
}

/// A prepared integration whose touched entries are on disk and whose
/// intent is persisted. The checkout's `index.lock` is still held by
/// `prepared`; the branch, the real index contents and every ref are
/// unchanged. Finishing or rolling back is up to later phases.
#[derive(Debug)]
pub struct AppliedIntegration {
    pub prepared: PreparedIntegration,
    /// Operation id recorded in the intent file.
    pub operation_id: String,
    /// Intent file in the checkout's per-worktree Git directory.
    pub intent_path: PathBuf,
    /// Stat data of every file apply wrote, taken right after writing it.
    /// Finalization gives an index entry fresh stat data only when its file
    /// still has exactly this stat, so later edits (by a hook, say) stay
    /// visible to `git status`.
    pub written: std::collections::BTreeMap<String, gix::index::entry::Stat>,
}

/// Outcome of
/// [`RepositoryOps::apply_integration`](crate::worktree::RepositoryOps::apply_integration).
#[derive(Debug)]
pub enum ApplyOutcome {
    Applied(Box<AppliedIntegration>),
    /// A precondition failed before any file was written; the index lock was
    /// released.
    Refused(Refusal),
}

/// Input of
/// [`RepositoryOps::finalize_integration`](crate::worktree::RepositoryOps::finalize_integration):
/// the applied files and the hook phase's private index and message.
#[derive(Debug)]
pub struct FinalizeRequest {
    pub applied: AppliedIntegration,
    pub hooked: HookedIntegration,
    /// Create-only record naming the commit that integrated this result,
    /// `refs/qmt/integrations/<workspace>/<result>`.
    pub integration_ref: String,
    /// Simulated process death inside finalization (fault injection only);
    /// production callers pass `None`.
    #[doc(hidden)]
    pub fault: Option<FinalizeFault>,
}

/// Stages of finalization at which a crash can be simulated.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeStage {
    /// After the hooks, before the commit object is written.
    Commit,
    /// After the commit is recorded in the intent, before the reference
    /// transaction.
    RefTransaction,
    /// After the reference transaction, before the real index is updated.
    IndexUpdate,
}

/// A simulated crash: finalization stops at `stage` the way a killed
/// process would (no rollback, the `index.lock` file stays behind) and
/// returns `error`.
#[doc(hidden)]
#[derive(Debug)]
pub struct FinalizeFault {
    pub stage: FinalizeStage,
    pub error: WorktreeError,
}

/// Outcome of the finalization phase. Every variant other than `Integrated`
/// leaves the branch unchanged, the touched paths restored, the intent
/// removed and the index lock released.
#[derive(Debug, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Integrated(IntegratedCommit),
    /// The integration record already existed; no second commit was made.
    AlreadyIntegrated {
        commit: String,
    },
    Refused(Refusal),
}

/// Outcome of
/// [`RepositoryOps::recover_integration`](crate::worktree::RepositoryOps::recover_integration).
#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// No unfinished integration exists on the checkout.
    NothingPending,
    /// The branch was still at the recorded `HEAD`: the touched paths were
    /// restored to it and the intent removed. Nothing was committed.
    RolledBack {
        operation_id: String,
        paths: Vec<String>,
    },
    /// The branch already pointed at the recorded commit: the real index
    /// was brought in line with it and the intent removed.
    Completed {
        operation_id: String,
        integrated: IntegratedCommit,
    },
    /// Recovery could not proceed safely (a foreign `index.lock`, or a
    /// branch that moved to neither recorded state). Nothing was changed
    /// and the intent is kept.
    Refused(Refusal),
}

/// Why a phase stopped early: a precondition failed (nothing changed and
/// the caller reports it), or the repository could not be read at all.
#[derive(Debug)]
pub(crate) enum Stop {
    Refused(Refusal),
    Failed(WorktreeError),
}

impl From<WorktreeError> for Stop {
    fn from(err: WorktreeError) -> Self {
        Self::Failed(err)
    }
}

impl From<Refusal> for Stop {
    fn from(refusal: Refusal) -> Self {
        Self::Refused(refusal)
    }
}

pub(crate) fn refuse(code: &'static str, message: impl Into<String>) -> Stop {
    Refusal::new(code, message).into()
}

fn refused(code: &'static str, message: impl Into<String>) -> WorktreeResult<IntegrationOutcome> {
    Ok(IntegrationOutcome::Refused(Refusal::new(code, message)))
}

// ---------------------------------------------------------------------------
// Explicit acceptance of a delegate's retained result
// ---------------------------------------------------------------------------

/// Records which commit integrated a result:
/// `refs/qmt/integrations/<workspace-id>/<result-commit>`. Create-only, so a
/// repeated acceptance finds it and reports the original commit.
pub const INTEGRATION_REF_PREFIX: &str = "refs/qmt/integrations";

/// The planner's decision: accept this reviewed result. Everything else
/// (workspace, baseline, target checkout, commit message) is resolved from
/// durable state, never from model-supplied paths.
#[derive(Debug, Clone)]
pub struct AcceptRequest<'a> {
    pub delegation_id: &'a str,
    /// The reviewed result commit (full id or an unambiguous prefix of at
    /// least 7 hex digits). Pins acceptance to what was reviewed.
    pub result_commit: &'a str,
    /// Session asking for the integration; must be the delegation's parent.
    pub requesting_session_id: &'a str,
    /// The requesting session's effective cwd; its checkout is the target.
    pub target_cwd: &'a Path,
    pub objective: &'a str,
    /// Durable status of the delegation; only a `Complete` delegation may be
    /// accepted. Callers read it from the session store.
    pub delegation_status: crate::session::domain::DelegationStatus,
    /// The requesting session's checkout mutation gate. When `None` (or
    /// bound to a different checkout) the checkout-keyed gate under the
    /// service's storage root is used, so acceptance always serializes.
    pub mutation_gate: Option<&'a CheckoutMutationGate>,
}

/// Everything the serialized transaction needs, resolved from durable state.
struct AcceptPlan {
    target_root: PathBuf,
    common_dir: PathBuf,
    baseline: Baseline,
    result_commit: String,
    integration_ref: String,
    message: String,
}

fn same_checkout(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

impl WorkspaceService {
    /// Integrate an accepted delegate result into the parent's checkout as
    /// one commit on its current branch. Deterministic; see the module docs.
    pub async fn accept_delegation_result(
        &self,
        request: &AcceptRequest<'_>,
    ) -> WorktreeResult<IntegrationOutcome> {
        if request.delegation_status != crate::session::domain::DelegationStatus::Complete {
            return refused(
                "delegation_not_complete",
                format!(
                    "delegation {} is {}; only a completed delegation can be accepted",
                    request.delegation_id, request.delegation_status
                ),
            );
        }
        let wanted = request.result_commit.to_ascii_lowercase();
        if wanted.len() < 7 || !wanted.bytes().all(|b| b.is_ascii_hexdigit()) {
            return refused(
                "invalid_result_commit",
                "result_commit must be the reviewed commit id (at least 7 hex digits)",
            );
        }
        let Some(record) = self
            .registry()
            .get_workspace_by_delegation(request.delegation_id)
            .await?
        else {
            return refused(
                "no_isolated_workspace",
                "this delegation has no isolated workspace; it worked in the shared checkout, \
                 so there is nothing to integrate",
            );
        };
        if record.ownership.parent_session_id.as_deref() != Some(request.requesting_session_id) {
            return refused(
                "not_parent_session",
                "only the session that created the delegation may accept its result",
            );
        }
        let Some(baseline) = record.baseline.clone() else {
            return refused("no_baseline", "the workspace has no recorded baseline");
        };
        let results = self.registry().list_results(&record.id).await?;
        let mut matching: Vec<&str> = results
            .iter()
            .filter_map(|r| r.result_commit.as_deref())
            .filter(|c| c.starts_with(&wanted))
            .collect();
        matching.sort_unstable();
        matching.dedup();
        if matching.len() > 1 {
            return Ok(IntegrationOutcome::Refused(
                Refusal::new(
                    "ambiguous_result_commit",
                    format!(
                        "{} matches more than one retained result; give more digits",
                        request.result_commit
                    ),
                )
                .with_paths(matching.iter().map(|c| c.to_string()).collect()),
            ));
        }
        let Some(result_commit) = matching.first().copied() else {
            return refused(
                "result_not_found",
                format!(
                    "workspace {} has no retained result matching {}",
                    record.id, request.result_commit
                ),
            );
        };

        let target = resolve_repository(request.target_cwd)?;
        if target.identity.common_dir != record.repository.common_dir {
            return refused(
                "wrong_repository",
                "the requesting session's checkout belongs to a different repository",
            );
        }
        let own_checkout = std::fs::canonicalize(&record.checkout_path)
            .unwrap_or_else(|_| record.checkout_path.clone());
        if target.checkout.root == own_checkout {
            return refused(
                "wrong_target",
                "the requesting session runs in the delegate's own workspace",
            );
        }

        let baseline_commit = baseline.baseline_commit.clone();
        let plan = AcceptPlan {
            target_root: target.checkout.root.clone(),
            common_dir: record.repository.common_dir.clone(),
            baseline,
            integration_ref: format!("{INTEGRATION_REF_PREFIX}/{}/{result_commit}", record.id),
            message: format!(
                "{}\n\nQueryMT-Delegation: {}\nQueryMT-Workspace: {}\nQueryMT-Result: \
                 {result_commit}\nQueryMT-Baseline: {}\n",
                sanitize_subject(request.objective),
                request.delegation_id,
                record.id,
                baseline_commit
            ),
            result_commit: result_commit.to_string(),
        };

        // Serialize on the requesting session's gate (or the checkout-keyed
        // fallback) for the whole transaction.
        let fallback;
        let gate: &CheckoutMutationGate = match request.mutation_gate {
            Some(gate) if same_checkout(gate.checkout_root(), &plan.target_root) => gate,
            _ => {
                fallback = CheckoutMutationGate::new(self.storage_root(), &plan.target_root)?;
                &fallback
            }
        };
        gate.run(self.integrate_under_gate(&plan)).await?
    }

    /// The transaction: recover, replay guard, prepare, apply, hooks,
    /// finalize. Callers hold the checkout mutation gate.
    async fn integrate_under_gate(&self, plan: &AcceptPlan) -> WorktreeResult<IntegrationOutcome> {
        let ops = self.ops();

        // 1. An earlier acceptance that did not finish is resolved first.
        match ops.recover_integration(&plan.target_root).await? {
            RecoveryOutcome::NothingPending => {}
            RecoveryOutcome::RolledBack {
                operation_id,
                paths,
            } => tracing::warn!(
                operation_id,
                ?paths,
                "rolled back an unfinished result integration before accepting"
            ),
            RecoveryOutcome::Completed {
                operation_id,
                integrated,
            } => tracing::warn!(
                operation_id,
                commit = %integrated.commit,
                "completed an unfinished result integration before accepting"
            ),
            RecoveryOutcome::Refused(refusal) => {
                return Ok(IntegrationOutcome::Refused(refusal));
            }
        }

        // 2. Replay guard: a recorded integration ref names the original
        // commit. (Checked after recovery, which may have just completed
        // this very result.)
        if let Some(commit) = ops
            .resolve_commit(&plan.common_dir, &plan.integration_ref)
            .await?
        {
            // Only a record whose commit is on the target branch proves the
            // result is integrated there; anything else (a branch reset, or
            // a record left by some other inconsistency) must not silently
            // block this result forever.
            let reachable = match ops.head_commit(&plan.target_root).await? {
                Some(head) => ops.is_ancestor(&plan.common_dir, &commit, &head).await?,
                None => false,
            };
            if reachable {
                return Ok(IntegrationOutcome::AlreadyIntegrated { commit });
            }
            return Ok(IntegrationOutcome::Refused(Refusal::new(
                "integration_record_unreachable",
                format!(
                    "{} records commit {commit} for this result, but that commit is not on the \
                     checkout's current branch (was the branch reset?). Nothing was changed; \
                     delete that ref to accept the result again",
                    plan.integration_ref
                ),
            )));
        }

        // 3. Preflight, delta, merge, local-work protection (holds the lock).
        let prepared = match ops
            .prepare_integration(PrepareRequest {
                checkout: plan.target_root.clone(),
                baseline_commit: plan.baseline.baseline_commit.clone(),
                result_commit: plan.result_commit.clone(),
                source_head: Some(plan.baseline.source_head.clone()),
            })
            .await?
        {
            PrepareOutcome::Ready(prepared) => prepared,
            PrepareOutcome::NoChanges => return Ok(IntegrationOutcome::NoChanges),
            PrepareOutcome::Conflicted { paths } => {
                return Ok(IntegrationOutcome::Conflicted { paths });
            }
            PrepareOutcome::Refused(refusal) => return Ok(IntegrationOutcome::Refused(refusal)),
        };

        // 4. Persist the intent and write the touched files.
        let applied = match ops.apply_integration(prepared).await {
            Ok(ApplyOutcome::Applied(applied)) => *applied,
            Ok(ApplyOutcome::Refused(refusal)) => return Ok(IntegrationOutcome::Refused(refusal)),
            Err(err) => return Err(self.after_failure(&plan.target_root, err).await),
        };

        // 5. Hooks (no bypass). A rejection leaves the applied files in
        // place; they are rolled back here, after the lock is released.
        let hooks = ops
            .run_integration_hooks(HookRequest::for_applied(&applied, plan.message.clone()))
            .await;
        let hooked = match hooks {
            Ok(HookOutcome::Passed(hooked)) => hooked,
            Ok(HookOutcome::Rejected(rejection)) => {
                let refusal = Refusal::new(
                    "commit_rejected",
                    format!(
                        "the {} hook {}: {}",
                        rejection.hook,
                        rejection.status,
                        rejection.output.trim()
                    ),
                );
                return rolled_back(applied, IntegrationOutcome::Refused(refusal)).await;
            }
            Ok(HookOutcome::Refused(refusal)) => {
                return rolled_back(applied, IntegrationOutcome::Refused(refusal)).await;
            }
            Err(err) => {
                return Err(match abandon(applied).await {
                    Ok(()) => err,
                    Err(rollback_err) => recovery_failed(&err, &rollback_err),
                });
            }
        };

        // 6. Commit, one reference transaction, real index. Finalize rolls
        // back its own refusals; a failure needs recovery, which either
        // restores the checkout or completes an index update that was cut
        // short after the branch moved.
        let finalized = ops
            .finalize_integration(FinalizeRequest {
                applied,
                hooked,
                integration_ref: plan.integration_ref.clone(),
                fault: None,
            })
            .await;
        match finalized {
            Ok(FinalizeOutcome::Integrated(done)) => Ok(IntegrationOutcome::Integrated(done)),
            Ok(FinalizeOutcome::AlreadyIntegrated { commit }) => {
                Ok(IntegrationOutcome::AlreadyIntegrated { commit })
            }
            Ok(FinalizeOutcome::Refused(refusal)) => Ok(IntegrationOutcome::Refused(refusal)),
            Err(err) => match ops.recover_integration(&plan.target_root).await {
                Ok(RecoveryOutcome::Completed { integrated, .. }) => {
                    tracing::warn!(
                        error = %err,
                        commit = %integrated.commit,
                        "finalization failed after the branch moved; recovery completed the index \
                         update (post-commit was not run)"
                    );
                    Ok(IntegrationOutcome::Integrated(integrated))
                }
                Ok(RecoveryOutcome::NothingPending | RecoveryOutcome::RolledBack { .. }) => {
                    Err(err)
                }
                Ok(RecoveryOutcome::Refused(refusal)) => Err(recovery_pending(&err, &refusal)),
                Err(recovery_err) => Err(recovery_failed(&err, &recovery_err)),
            },
        }
    }

    /// Recover after a failed phase and return the error to report.
    async fn after_failure(&self, root: &Path, err: WorktreeError) -> WorktreeError {
        match self.ops().recover_integration(root).await {
            Ok(RecoveryOutcome::Refused(refusal)) => recovery_pending(&err, &refusal),
            Ok(_) => err,
            Err(recovery_err) => recovery_failed(&err, &recovery_err),
        }
    }
}

/// Roll back after a refusal that left applied files in place (still under
/// the transaction's own index lock), then report `outcome`. Failing to roll
/// back is an error, not a refusal: the checkout is not unchanged.
async fn rolled_back(
    applied: AppliedIntegration,
    outcome: IntegrationOutcome,
) -> WorktreeResult<IntegrationOutcome> {
    match abandon(applied).await {
        Ok(()) => Ok(outcome),
        Err(err) => Err(WorktreeError::InvalidOperation(format!(
            "the acceptance was refused but rolling the checkout back failed: {err}; the next \
             acceptance retries the recovery"
        ))),
    }
}

/// [`crate::worktree::apply::abandon_applied`] on the blocking pool.
async fn abandon(applied: AppliedIntegration) -> WorktreeResult<()> {
    tokio::task::spawn_blocking(move || crate::worktree::apply::abandon_applied(applied))
        .await
        .map_err(crate::worktree::repo_ops::native_err)?
}

fn recovery_pending(err: &WorktreeError, refusal: &Refusal) -> WorktreeError {
    WorktreeError::InvalidOperation(format!(
        "{err}; recovery is pending and was not possible yet: {} ({})",
        refusal.message, refusal.code
    ))
}

fn recovery_failed(err: &WorktreeError, recovery_err: &WorktreeError) -> WorktreeError {
    WorktreeError::InvalidOperation(format!(
        "{err}; recovering the checkout also failed ({recovery_err}); the next acceptance \
         retries the recovery"
    ))
}

/// First line of a free-form objective, safe for a commit subject.
pub fn sanitize_subject(text: &str) -> String {
    const MAX: usize = 72;
    let line: String = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("delegated change")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if line.chars().count() <= MAX {
        line
    } else {
        let mut cut: String = line.chars().take(MAX - 1).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_is_a_single_bounded_line() {
        assert_eq!(
            sanitize_subject("\n  Fix the thing\nmore detail"),
            "Fix the thing"
        );
        let long = sanitize_subject(&"x".repeat(200));
        assert_eq!(long.chars().count(), 72);
        assert!(long.ends_with('…'));
    }
}
