//! Domain types for managed Git worktree workspaces.
//!
//! These types form the vocabulary of the worktree isolation feature:
//! identity, baseline mode, lifecycle, execution policy, ownership, and
//! result provenance. They are pure data: Git execution, provisioning, and
//! persistence live in sibling modules and the workspace repository.
//!
//! Design invariants (see `openspec/changes/add-git-worktree-isolation/design.md`):
//! - Repository identity is derived from the canonical Git common directory,
//!   so linked worktrees of one project group together while keeping distinct
//!   checkout identities.
//! - Omitted isolation options always resolve to existing local behavior;
//!   isolation is strictly opt-in.
//! - Isolated parents isolate their delegates by default; local parents keep
//!   legacy shared execution unless isolation is explicitly requested.
//! - Lifecycle states are durable and explicit (`Provisioning`, `Ready`,
//!   `Failed`, `Missing`, `Removing`, `Removed`); dirty/clean Git status is
//!   observed live, never persisted as lifecycle state.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use time::OffsetDateTime;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Unique durable identifier of a managed workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Generate a fresh workspace identifier (UUID v7, time-ordered).
    pub fn generate() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    /// Wrap an existing identifier string without validation.
    ///
    /// Callers loading persisted records use this; constructors that mint new
    /// identities should prefer [`WorkspaceId::generate`].
    pub fn from_raw(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for WorkspaceId {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

/// Stable, node-local identity of the project repository a workspace belongs
/// to. Derived from the canonical Git common directory so that linked
/// worktrees of the same project share one identity while remaining distinct
/// checkouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryIdentity {
    /// Canonical absolute path of the repository's common directory
    /// (typically `<project>/.git`), resolved through
    /// `git rev-parse --git-common-dir`.
    pub common_dir: PathBuf,
    /// Original project path the workspace was created from. Kept for
    /// display and diagnostics; identity comparisons use `common_dir`.
    pub origin_path: PathBuf,
    /// Human-readable project name derived from the repository directory.
    pub project_name: String,
}

impl RepositoryIdentity {
    /// Derive a display name from the origin path's final component.
    pub fn project_name_from_origin(origin_path: &std::path::Path) -> String {
        origin_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| origin_path.to_string_lossy().into_owned())
    }
}

/// Branch name assigned to a managed workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ManagedBranch(String);

impl ManagedBranch {
    /// Prefix used for branches created and owned by QueryMT.
    pub const MANAGED_PREFIX: &'static str = "qmt/workspace/";

    /// Compose the managed branch for a workspace: `qmt/workspace/<id>`.
    pub fn for_workspace(workspace_id: &WorkspaceId) -> Self {
        Self(format!("{}{}", Self::MANAGED_PREFIX, workspace_id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ManagedBranch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl ManagedBranch {
    /// Wrap an existing branch name loaded from durable storage.
    pub fn from_raw(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Managed baseline ref holding a workspace's immutable baseline commit.
///
/// `refs/qmt/baselines/<workspace-id>` keeps the baseline reachable across
/// process restarts and Git garbage collection while any workspace or
/// result record depends on it. Removal retains (never deletes) these refs
/// and reports them.
pub fn baseline_ref_for(workspace_id: &WorkspaceId) -> String {
    format!("refs/qmt/baselines/{workspace_id}")
}

// ---------------------------------------------------------------------------
// Baseline mode
// ---------------------------------------------------------------------------

/// What the baseline of a new isolated workspace is captured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineMode {
    /// Capture a committed ref only. Uncommitted local edits in the source
    /// are excluded and disclosed.
    #[default]
    Commit,
    /// Capture the source's current working contents (tracked working files,
    /// tracked deletions, and non-ignored untracked files) into an immutable
    /// private baseline without touching the source index, HEAD, or files.
    CurrentWorkingTree,
}

impl fmt::Display for BaselineMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaselineMode::Commit => f.write_str("commit"),
            BaselineMode::CurrentWorkingTree => f.write_str("current_working_tree"),
        }
    }
}

impl FromStr for BaselineMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "commit" => Ok(BaselineMode::Commit),
            "current_working_tree" => Ok(BaselineMode::CurrentWorkingTree),
            other => Err(format!("unknown baseline mode: {other}")),
        }
    }
}

/// Immutable baseline identity captured for a workspace.
///
/// `source_head` is the commit HEAD pointed at when capture ran;
/// `baseline_commit` is the commit isolated work is measured against. When
/// the captured tree equals `source_head`, the baseline reuses that commit
/// instead of creating a private one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub mode: BaselineMode,
    /// Full object id of the source HEAD at capture time.
    pub source_head: String,
    /// Full object id of the immutable baseline commit (may equal
    /// `source_head` when the tree matched HEAD).
    pub baseline_commit: String,
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Durable lifecycle state of a managed workspace.
///
/// Dirty/clean working-tree status is deliberately absent: it changes outside
/// QueryMT and is observed live through Git, never persisted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLifecycle {
    /// Creation intent persisted; branch/checkout allocation in progress.
    Provisioning,
    /// Checkout allocated, bound, and ready for session execution.
    Ready,
    /// Provisioning or operation failed; `last_error` holds diagnostics.
    Failed,
    /// The checkout directory disappeared (deleted externally).
    Missing,
    /// Explicitly retired execution; binding/history are retained, resume is refused.
    Archived,
    /// Explicit removal in progress.
    Removing,
    /// Removal completed; the checkout path must never be reused.
    Removed,
}

impl WorkspaceLifecycle {
    /// States from which a workspace can be attached to a session.
    pub fn is_attachable(self) -> bool {
        matches!(self, WorkspaceLifecycle::Ready)
    }

    /// Whether a lifecycle transition from `self` to `next` is meaningful.
    pub fn can_transition_to(self, next: WorkspaceLifecycle) -> bool {
        use WorkspaceLifecycle::*;
        matches!(
            (self, next),
            (Provisioning, Ready)
                | (Provisioning, Failed)
                | (Ready, Missing)
                | (Ready, Archived)
                | (Missing, Archived)
                | (Failed, Archived)
                | (Archived, Removing)
                | (Ready, Removing)
                | (Failed, Removing)
                | (Missing, Removing)
                | (Removing, Removed)
                | (Failed, Provisioning) // idempotent retry after recovery
        )
    }
}

impl fmt::Display for WorkspaceLifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            WorkspaceLifecycle::Provisioning => "provisioning",
            WorkspaceLifecycle::Ready => "ready",
            WorkspaceLifecycle::Failed => "failed",
            WorkspaceLifecycle::Missing => "missing",
            WorkspaceLifecycle::Archived => "archived",
            WorkspaceLifecycle::Removing => "removing",
            WorkspaceLifecycle::Removed => "removed",
        };
        f.write_str(s)
    }
}

impl FromStr for WorkspaceLifecycle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "provisioning" => Ok(WorkspaceLifecycle::Provisioning),
            "ready" => Ok(WorkspaceLifecycle::Ready),
            "failed" => Ok(WorkspaceLifecycle::Failed),
            "missing" => Ok(WorkspaceLifecycle::Missing),
            "archived" => Ok(WorkspaceLifecycle::Archived),
            "removing" => Ok(WorkspaceLifecycle::Removing),
            "removed" => Ok(WorkspaceLifecycle::Removed),
            other => Err(format!("unknown workspace lifecycle state: {other}")),
        }
    }
}

/// Durable provisioning progress of an intent, persisted after each
/// completed allocation step so interrupted creation can be replayed from
/// the last recorded boundary.
///
/// The phase is strictly forward-only while the workspace is
/// [`WorkspaceLifecycle::Provisioning`]; reaching [`ProvisioningPhase::CheckoutCreated`]
/// means every Git resource exists and only the final lifecycle transition to
/// `Ready` remains. Rows recorded before this field existed have `None` and
/// are reconciled from the presence of a bound baseline instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningPhase {
    /// The replayable intent (source, requested ref/policy, relative cwd,
    /// operation id, intended branch/checkout) is durably persisted; nothing
    /// has been captured or allocated yet.
    IntentPersisted,
    /// The immutable baseline identity is durably bound to the workspace.
    BaselineBound,
    /// The retained baseline ref has been published at the bound baseline.
    BaselineRefPublished,
    /// The managed branch exists at the baseline commit.
    BranchCreated,
    /// The linked worktree checkout exists; only `Ready` remains.
    CheckoutCreated,
}

impl ProvisioningPhase {
    /// Whether `next` is a forward step from `self`.
    pub fn can_advance_to(self, next: ProvisioningPhase) -> bool {
        next > self
    }
}

impl fmt::Display for ProvisioningPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProvisioningPhase::IntentPersisted => "intent_persisted",
            ProvisioningPhase::BaselineBound => "baseline_bound",
            ProvisioningPhase::BaselineRefPublished => "baseline_ref_published",
            ProvisioningPhase::BranchCreated => "branch_created",
            ProvisioningPhase::CheckoutCreated => "checkout_created",
        };
        f.write_str(s)
    }
}

impl FromStr for ProvisioningPhase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "intent_persisted" => Ok(ProvisioningPhase::IntentPersisted),
            "baseline_bound" => Ok(ProvisioningPhase::BaselineBound),
            "baseline_ref_published" => Ok(ProvisioningPhase::BaselineRefPublished),
            "branch_created" => Ok(ProvisioningPhase::BranchCreated),
            "checkout_created" => Ok(ProvisioningPhase::CheckoutCreated),
            other => Err(format!("unknown provisioning phase: {other}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Execution policy
// ---------------------------------------------------------------------------

/// Where a session executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Execute in the caller-provided working directory (existing behavior).
    #[default]
    Local,
    /// Execute in a managed, isolated Git linked worktree.
    Worktree,
}

impl fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionMode::Local => f.write_str("local"),
            ExecutionMode::Worktree => f.write_str("worktree"),
        }
    }
}

impl FromStr for ExecutionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(ExecutionMode::Local),
            "worktree" => Ok(ExecutionMode::Worktree),
            other => Err(format!("unknown execution mode: {other}")),
        }
    }
}

/// Delegation isolation requested for a delegate relative to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationIsolation {
    /// Resolve from the parent's execution mode: isolated parents isolate
    /// delegates by default; local parents keep legacy shared execution.
    #[default]
    Inherit,
    /// Explicitly share the parent's checkout (disclosed as shared).
    Shared,
    /// Explicitly request a private checkout for this delegate.
    Isolated,
}

/// Isolation resolved for execution. Shared execution is the existing
/// behavior and is always disclosed as shared, never presented as isolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedIsolation {
    Shared,
    Isolated,
}

/// Resolve the delegation isolation policy.
///
/// Rules (design decision 2):
/// - absent/inherited request + local parent → `Shared` (legacy behavior)
/// - absent/inherited request + isolated parent → `Isolated`
/// - explicit request wins, including an explicit `Shared` override on an
///   isolated parent (callers must disclose that as shared execution).
pub fn resolve_delegation_isolation(
    parent_mode: ExecutionMode,
    request: DelegationIsolation,
) -> ResolvedIsolation {
    match request {
        DelegationIsolation::Isolated => ResolvedIsolation::Isolated,
        DelegationIsolation::Shared => ResolvedIsolation::Shared,
        DelegationIsolation::Inherit => match parent_mode {
            ExecutionMode::Local => ResolvedIsolation::Shared,
            ExecutionMode::Worktree => ResolvedIsolation::Isolated,
        },
    }
}

/// Typed options for creating an isolated workspace. Absent options preserve
/// local behavior: a `None` request (or `Default`) never creates a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceCreationOptions {
    /// Local or worktree execution. Defaults to `Local`.
    pub execution: ExecutionMode,
    /// What the baseline is captured from. Defaults to `Commit` (current
    /// HEAD), which excludes uncommitted local edits.
    pub baseline: BaselineMode,
    /// Explicit base ref (branch, tag, or commit). Absent means the source
    /// repository's current HEAD.
    pub base_ref: Option<String>,
}

impl WorkspaceCreationOptions {
    /// Options preserving existing local execution unchanged.
    pub fn local() -> Self {
        Self::default()
    }

    /// Whether these options request an isolated worktree.
    pub fn requests_isolation(&self) -> bool {
        self.execution == ExecutionMode::Worktree
    }
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

/// Durable ownership of a workspace.
///
/// A managed workspace has at most one writable session owner; multiple
/// connections to that same session are allowed, a second writable session
/// attachment is not.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceOwnership {
    /// Public id of the session that owns this workspace, if bound yet.
    pub owner_session_id: Option<String>,
    /// Workspace this workspace was derived from (delegation parent or
    /// conversation fork source), if any.
    pub parent_workspace_id: Option<WorkspaceId>,
    /// Session that requested this workspace (delegation parent or fork
    /// source session), if any.
    pub parent_session_id: Option<String>,
    /// Delegation this workspace was allocated for, if any.
    pub delegation_id: Option<String>,
    /// Parallel delegation batch this workspace belongs to. Delegates in one
    /// batch share one captured baseline.
    pub delegation_batch_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Result provenance
// ---------------------------------------------------------------------------

/// Verification outcome attached to a retained delegate result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultVerificationStatus {
    Pending,
    Passed,
    Failed,
    Skipped,
}

impl fmt::Display for ResultVerificationStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ResultVerificationStatus::Pending => "pending",
            ResultVerificationStatus::Passed => "passed",
            ResultVerificationStatus::Failed => "failed",
            ResultVerificationStatus::Skipped => "skipped",
        };
        f.write_str(s)
    }
}

impl FromStr for ResultVerificationStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(ResultVerificationStatus::Pending),
            "passed" => Ok(ResultVerificationStatus::Passed),
            "failed" => Ok(ResultVerificationStatus::Failed),
            "skipped" => Ok(ResultVerificationStatus::Skipped),
            other => Err(format!("unknown verification status: {other}")),
        }
    }
}

/// Provenance of a retained result (delegate or top-level workspace)
/// relative to its baseline.
///
/// `baseline_commit` is inherited from the workspace baseline;
/// `result_commit` is the immutable capture of uncommitted working output,
/// if capture succeeded. Absent results never imply integration: nothing in
/// this type causes a parent merge or claims the result was applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultProvenance {
    /// Baseline the result is measured against (matches the workspace
    /// baseline at delegation time).
    pub baseline_commit: String,
    /// Immutable result commit capturing uncommitted output, when capture
    /// succeeded.
    pub result_commit: Option<String>,
    /// Retained ref holding `result_commit` (e.g. `refs/qmt/results/<id>`).
    pub result_ref: Option<String>,
    /// Verification outcome, when verification ran.
    pub verification: Option<ResultVerificationStatus>,
    /// When the result was captured.
    #[serde(with = "time::serde::rfc3339")]
    pub captured_at: OffsetDateTime,
    /// HEAD of the checkout at capture time, recorded separately from live
    /// HEAD so review stays accurate after later branch movement. Rows
    /// persisted before this field existed read as `None`; review derives
    /// from the workspace baseline in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_head: Option<String>,
}

/// How a path changed between two immutable trees (baseline → result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl fmt::Display for ChangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
        };
        f.write_str(s)
    }
}

impl FromStr for ChangeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "added" => Ok(ChangeKind::Added),
            "modified" => Ok(ChangeKind::Modified),
            "deleted" => Ok(ChangeKind::Deleted),
            other => Err(format!("unknown change kind: {other}")),
        }
    }
}

/// One baseline-relative change: a repository-relative path and its kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedPath {
    /// Repository-relative path, forward-slash separated.
    pub path: String,
    pub kind: ChangeKind,
}

/// Immutable baseline-relative review of a published result.
///
/// Every field except the live-state disclosure derives from the retained
/// refs, so review is identical after restart, later edits, or further
/// publication. `baseline_includes_uncommitted_edits` marks results whose
/// baseline captured a dirty parent: merging the private branch directly
/// would also import those inherited edits, so integration must apply the
/// listed delta deliberately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultReview {
    /// Baseline commit the delta is measured against.
    pub baseline_commit: String,
    /// Immutable result commit under review.
    pub result_commit: String,
    /// Retained ref pinning [`Self::result_commit`], when publication wrote
    /// one.
    pub result_ref: Option<String>,
    /// HEAD of the checkout at capture time — distinct from live HEAD.
    pub captured_head: String,
    /// Verification outcome, when known.
    pub verification: Option<ResultVerificationStatus>,
    /// When the result was captured.
    #[serde(with = "time::serde::rfc3339")]
    pub captured_at: OffsetDateTime,
    /// Baseline-relative changes (added/modified/deleted), sorted by path.
    pub changed_paths: Vec<ChangedPath>,
    /// Whether the baseline captured uncommitted parent edits; carries the
    /// dirty-baseline merge warning.
    pub baseline_includes_uncommitted_edits: bool,
}

impl ResultReview {
    /// Human-readable review summary used in completion/summarizer output.
    /// Lists only the baseline-relative delta and never claims the result
    /// was integrated.
    pub fn summary_text(&self) -> String {
        let mut lines = Vec::with_capacity(self.changed_paths.len().max(1));
        for changed in &self.changed_paths {
            lines.push(format!("  {} ({})", changed.path, changed.kind));
        }
        if lines.is_empty() {
            lines.push("  (no baseline-relative changes)".to_string());
        }
        let mut text = format!(
            "Baseline-relative changes (immutable result {}):\n{}",
            self.result_commit,
            lines.join("\n")
        );
        if self.baseline_includes_uncommitted_edits {
            text.push_str(
                "\nWARNING: the baseline includes inherited uncommitted parent edits; \
                 merging the private branch directly would import them too — apply \
                 only the listed delta deliberately.",
            );
        }
        text
    }
}

// ---------------------------------------------------------------------------
// Workspace record
// ---------------------------------------------------------------------------

/// A persisted managed workspace.
///
/// This is the durable record shared between the workspace service, the
/// session materializer, and the management surface. `checkout_path` and
/// `relative_cwd` are the execution identity; `repository` is the project
/// identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub repository: RepositoryIdentity,
    /// Absolute path of the linked worktree checkout (durable storage).
    pub checkout_path: PathBuf,
    /// Branch checked out in the workspace.
    pub branch: ManagedBranch,
    /// Session cwd relative to the checkout root (preserves subdirectory
    /// sessions). Empty for checkout root.
    pub relative_cwd: PathBuf,
    /// Execution mode this workspace serves (always `Worktree` for managed
    /// records; carried so local sessions can be modeled uniformly).
    pub execution: ExecutionMode,
    /// Captured baseline identity, present once capture completed.
    pub baseline: Option<Baseline>,
    /// Durable lifecycle state.
    pub lifecycle: WorkspaceLifecycle,
    pub ownership: WorkspaceOwnership,
    /// Last failure diagnostic for `Failed`/`Missing` states.
    pub last_error: Option<String>,
    /// Baseline policy requested at intent time (`commit` or
    /// `current_working_tree`). Persisted even while `baseline` is `None`
    /// so baseline-absent recovery knows which capture to retry.
    pub requested_baseline_mode: BaselineMode,
    /// Explicit ref requested at intent time, if any. Re-resolved during
    /// baseline-absent recovery; the resolved commit itself is never
    /// fabricated.
    pub requested_base_ref: Option<String>,
    /// Unique id of the provisioning operation. Generated with the intent so
    /// replay attempts can be identified and never re-generate resources
    /// under a different identity.
    pub operation_id: String,
    /// Last completed provisioning step. `None` for rows recorded before
    /// phase tracking existed; `Some` only while `lifecycle` is
    /// `Provisioning` (cleared once the workspace becomes ready).
    pub provisioning_phase: Option<ProvisioningPhase>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl WorkspaceRecord {
    /// Effective session cwd inside the workspace.
    pub fn session_cwd(&self) -> PathBuf {
        if self.relative_cwd.as_os_str().is_empty() {
            self.checkout_path.clone()
        } else {
            self.checkout_path.join(&self.relative_cwd)
        }
    }

    /// Resolved delegation isolation for delegates spawned by the session
    /// that owns this record. Local records keep legacy shared delegation.
    pub fn delegation_isolation_default(&self) -> ResolvedIsolation {
        match self.execution {
            ExecutionMode::Local => ResolvedIsolation::Shared,
            ExecutionMode::Worktree => ResolvedIsolation::Isolated,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn roundtrip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    // -- Identity ----------------------------------------------------------

    #[test]
    fn workspace_id_is_unique_and_transparent_in_json() {
        let a = WorkspaceId::generate();
        let b = WorkspaceId::generate();
        assert_ne!(a, b, "v7 ids must be unique");

        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, format!("\"{a}\""));
        let back: WorkspaceId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn managed_branch_is_namespaced_per_workspace() {
        let id = WorkspaceId::from_raw("01933d2f-1111-7abc-8def-222222222222");
        let branch = ManagedBranch::for_workspace(&id);
        assert_eq!(
            branch.as_str(),
            "qmt/workspace/01933d2f-1111-7abc-8def-222222222222"
        );
        assert!(branch.as_str().starts_with(ManagedBranch::MANAGED_PREFIX));
        assert_ne!(
            branch,
            ManagedBranch::for_workspace(&WorkspaceId::generate())
        );
    }

    #[test]
    fn repository_identity_serializes_paths() {
        let identity = RepositoryIdentity {
            common_dir: PathBuf::from("/repo/.git"),
            origin_path: PathBuf::from("/repo"),
            project_name: RepositoryIdentity::project_name_from_origin(&PathBuf::from("/repo")),
        };
        let back = roundtrip(&identity);
        assert_eq!(back, identity);
        assert_eq!(back.project_name, "repo");
    }

    #[test]
    fn project_name_falls_back_for_root_paths() {
        assert_eq!(
            RepositoryIdentity::project_name_from_origin(std::path::Path::new("/")),
            "/"
        );
    }

    // -- Baseline mode ------------------------------------------------------

    #[test]
    fn baseline_mode_defaults_to_commit_and_serializes_snake_case() {
        assert_eq!(BaselineMode::default(), BaselineMode::Commit);
        assert_eq!(
            serde_json::to_string(&BaselineMode::Commit).unwrap(),
            "\"commit\""
        );
        assert_eq!(
            serde_json::to_string(&BaselineMode::CurrentWorkingTree).unwrap(),
            "\"current_working_tree\""
        );
        let back: BaselineMode = serde_json::from_str("\"current_working_tree\"").unwrap();
        assert_eq!(back, BaselineMode::CurrentWorkingTree);
    }

    #[test]
    fn baseline_roundtrip_preserves_mode_and_commits() {
        let baseline = Baseline {
            mode: BaselineMode::CurrentWorkingTree,
            source_head: "abc123".into(),
            baseline_commit: "def456".into(),
        };
        assert_eq!(roundtrip(&baseline), baseline);
    }

    // -- Lifecycle ----------------------------------------------------------

    #[test]
    fn lifecycle_serializes_snake_case_and_roundtrips() {
        for state in [
            WorkspaceLifecycle::Provisioning,
            WorkspaceLifecycle::Ready,
            WorkspaceLifecycle::Failed,
            WorkspaceLifecycle::Missing,
            WorkspaceLifecycle::Removing,
            WorkspaceLifecycle::Removed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: WorkspaceLifecycle = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state);
            assert_eq!(back.to_string(), json.trim_matches('"'));
        }
    }

    #[test]
    fn lifecycle_transitions_follow_provisioning_recovery_paths() {
        use WorkspaceLifecycle::*;
        assert!(Provisioning.can_transition_to(Ready));
        assert!(Provisioning.can_transition_to(Failed));
        assert!(Failed.can_transition_to(Provisioning), "retry allowed");
        assert!(Ready.can_transition_to(Missing));
        assert!(Ready.can_transition_to(Removing));
        assert!(Missing.can_transition_to(Removing));
        assert!(Removing.can_transition_to(Removed));

        assert!(!Ready.can_transition_to(Provisioning));
        assert!(!Removed.can_transition_to(Removing));
        assert!(!Removed.can_transition_to(Ready));

        assert!(Ready.is_attachable());
        assert!(!Provisioning.is_attachable());
        assert!(!Failed.is_attachable());
        assert!(!Removed.is_attachable());
    }

    // -- Policy -------------------------------------------------------------

    #[test]
    fn absent_options_preserve_local_behavior() {
        // A `None`/default request must never ask for isolation.
        let absent: Option<WorkspaceCreationOptions> = None;
        assert!(absent.is_none());
        assert!(!WorkspaceCreationOptions::default().requests_isolation());
        assert!(!WorkspaceCreationOptions::local().requests_isolation());
        assert_eq!(
            WorkspaceCreationOptions::default().execution,
            ExecutionMode::Local
        );
        assert_eq!(
            WorkspaceCreationOptions::default().baseline,
            BaselineMode::Commit
        );
        assert_eq!(WorkspaceCreationOptions::default().base_ref, None);
    }

    #[test]
    fn execution_mode_defaults_to_local_and_serializes_snake_case() {
        assert_eq!(ExecutionMode::default(), ExecutionMode::Local);
        assert_eq!(
            serde_json::to_string(&ExecutionMode::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionMode::Worktree).unwrap(),
            "\"worktree\""
        );
    }

    #[test]
    fn delegation_isolation_defaults_to_inherit() {
        assert_eq!(DelegationIsolation::default(), DelegationIsolation::Inherit);
    }

    #[test]
    fn resolution_preserves_legacy_shared_delegation_from_local_parents() {
        assert_eq!(
            resolve_delegation_isolation(ExecutionMode::Local, DelegationIsolation::Inherit),
            ResolvedIsolation::Shared,
            "local sessions keep legacy shared delegation when nothing is requested"
        );
    }

    #[test]
    fn isolated_parents_isolate_delegates_by_default() {
        assert_eq!(
            resolve_delegation_isolation(ExecutionMode::Worktree, DelegationIsolation::Inherit),
            ResolvedIsolation::Isolated,
            "isolated sessions isolate delegates unless overridden"
        );
    }

    #[test]
    fn explicit_requests_win_over_inheritance() {
        // Explicit isolation from a local parent is honored.
        assert_eq!(
            resolve_delegation_isolation(ExecutionMode::Local, DelegationIsolation::Isolated),
            ResolvedIsolation::Isolated
        );
        // Explicit shared override on an isolated parent resolves shared;
        // callers disclose this as shared execution.
        assert_eq!(
            resolve_delegation_isolation(ExecutionMode::Worktree, DelegationIsolation::Shared),
            ResolvedIsolation::Shared
        );
    }

    #[test]
    fn isolation_enums_roundtrip() {
        for req in [
            DelegationIsolation::Inherit,
            DelegationIsolation::Shared,
            DelegationIsolation::Isolated,
        ] {
            assert_eq!(roundtrip(&req), req);
        }
        assert_eq!(
            roundtrip(&ResolvedIsolation::Shared),
            ResolvedIsolation::Shared
        );
        assert_eq!(
            roundtrip(&ResolvedIsolation::Isolated),
            ResolvedIsolation::Isolated
        );
    }

    #[test]
    fn creation_options_roundtrip() {
        let opts = WorkspaceCreationOptions {
            execution: ExecutionMode::Worktree,
            baseline: BaselineMode::CurrentWorkingTree,
            base_ref: Some("feature-x".into()),
        };
        assert_eq!(roundtrip(&opts), opts);
    }

    // -- Ownership ----------------------------------------------------------

    #[test]
    fn ownership_defaults_to_unowned_and_roundtrips() {
        let ownership = WorkspaceOwnership::default();
        assert_eq!(ownership.owner_session_id, None);
        assert_eq!(ownership.parent_workspace_id, None);

        let owned = WorkspaceOwnership {
            owner_session_id: Some("sess-1".into()),
            parent_workspace_id: Some(WorkspaceId::from_raw("ws-parent")),
            parent_session_id: Some("sess-0".into()),
            delegation_id: Some("del-9".into()),
            delegation_batch_id: Some("batch-2".into()),
        };
        assert_eq!(roundtrip(&owned), owned);
    }

    // -- Result provenance ----------------------------------------------------

    #[test]
    fn result_provenance_roundtrip() {
        let provenance = ResultProvenance {
            baseline_commit: "base123".into(),
            result_commit: Some("res456".into()),
            result_ref: Some("refs/qmt/results/ws-1".into()),
            verification: Some(ResultVerificationStatus::Passed),
            captured_at: datetime!(2026-01-15 12:00:00 UTC),
        };
        assert_eq!(roundtrip(&provenance), provenance);
    }

    #[test]
    fn result_provenance_without_result_commit_roundtrips() {
        let provenance = ResultProvenance {
            baseline_commit: "base123".into(),
            result_commit: None,
            result_ref: None,
            verification: Some(ResultVerificationStatus::Failed),
            captured_at: datetime!(2026-01-15 12:00:00 UTC),
        };
        assert_eq!(roundtrip(&provenance), provenance);
    }

    #[test]
    fn verification_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&ResultVerificationStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&ResultVerificationStatus::Passed).unwrap(),
            "\"passed\""
        );
    }

    // -- Record -------------------------------------------------------------

    fn sample_record() -> WorkspaceRecord {
        WorkspaceRecord {
            id: WorkspaceId::from_raw("ws-1"),
            repository: RepositoryIdentity {
                common_dir: PathBuf::from("/repo/.git"),
                origin_path: PathBuf::from("/repo"),
                project_name: "repo".into(),
            },
            checkout_path: PathBuf::from("/data/worktrees/ws-1"),
            branch: ManagedBranch::for_workspace(&WorkspaceId::from_raw("ws-1")),
            relative_cwd: PathBuf::from("crates/agent"),
            execution: ExecutionMode::Worktree,
            baseline: Some(Baseline {
                mode: BaselineMode::Commit,
                source_head: "aaa".into(),
                baseline_commit: "aaa".into(),
            }),
            lifecycle: WorkspaceLifecycle::Ready,
            ownership: WorkspaceOwnership {
                owner_session_id: Some("sess-1".into()),
                ..Default::default()
            },
            last_error: None,
            requested_baseline_mode: BaselineMode::Commit,
            requested_base_ref: None,
            operation_id: "op-1".into(),
            provisioning_phase: None,
            created_at: datetime!(2026-01-15 12:00:00 UTC),
            updated_at: datetime!(2026-01-15 12:00:00 UTC),
        }
    }

    #[test]
    fn workspace_record_roundtrip() {
        let record = sample_record();
        assert_eq!(roundtrip(&record), record);
    }

    /// The true first persisted intent has no baseline yet: the requested
    /// policy, operation id, and phase round-trip without any fabricated
    /// capture.
    #[test]
    fn intent_only_record_roundtrips_without_baseline() {
        let record = WorkspaceRecord {
            baseline: None,
            lifecycle: WorkspaceLifecycle::Provisioning,
            requested_baseline_mode: BaselineMode::CurrentWorkingTree,
            requested_base_ref: None,
            provisioning_phase: Some(ProvisioningPhase::IntentPersisted),
            ..sample_record()
        };
        let back = roundtrip(&record);
        assert_eq!(back, record);
        assert!(back.baseline.is_none());
        assert_eq!(
            back.provisioning_phase,
            Some(ProvisioningPhase::IntentPersisted)
        );
    }

    #[test]
    fn provisioning_phase_serializes_snake_case_and_orders_forward() {
        let phase = ProvisioningPhase::BaselineBound;
        assert_eq!(phase.to_string(), "baseline_bound");
        assert_eq!(roundtrip(&phase), phase);
        assert!(ProvisioningPhase::IntentPersisted.can_advance_to(phase));
        assert!(!phase.can_advance_to(ProvisioningPhase::IntentPersisted));
        assert!(phase.can_advance_to(ProvisioningPhase::CheckoutCreated));
        assert_eq!(
            "checkout_created".parse::<ProvisioningPhase>().unwrap(),
            ProvisioningPhase::CheckoutCreated
        );
        assert!("nonsense".parse::<ProvisioningPhase>().is_err());
    }

    #[test]
    fn session_cwd_joins_relative_cwd() {
        let mut record = sample_record();
        assert_eq!(
            record.session_cwd(),
            PathBuf::from("/data/worktrees/ws-1/crates/agent")
        );

        record.relative_cwd = PathBuf::new();
        assert_eq!(record.session_cwd(), PathBuf::from("/data/worktrees/ws-1"));
    }

    #[test]
    fn record_default_delegation_matches_execution_mode() {
        let mut record = sample_record();
        assert_eq!(
            record.delegation_isolation_default(),
            ResolvedIsolation::Isolated
        );

        record.execution = ExecutionMode::Local;
        assert_eq!(
            record.delegation_isolation_default(),
            ResolvedIsolation::Shared,
            "local records keep legacy shared delegation"
        );
    }

    #[test]
    fn rfc3339_timestamps_survive_json() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("2026-01-15T12:00:00"));
    }
}
