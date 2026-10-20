//! Managed Git worktree workspaces.
//!
//! This module implements the `git-worktree-workspaces` capability: durable,
//! session-bound linked-worktree checkouts with explicit local versus
//! isolated execution policy.
//!
//! Layout:
//! - [`domain`]: pure domain types (identity, lifecycle, policy, provenance)
//! - [`git`]: injectable Git CLI adapter (explicit argv, controlled env)
//! - [`identity`]: repository/checkout identity and relative-cwd resolution
//! - [`lock`]: cross-process repository-administration locking
//! - [`repo_workspace`]: durable workspace registry (SQLite)
//! - [`allocate`]: provisioning and crash recovery of new workspaces
//! - [`lifecycle`]: live status, missing-workspace reconciliation, removal

pub mod allocate;
pub mod capture;
pub mod domain;
pub mod error;
pub mod gate;
#[cfg(test)]
pub(crate) mod git;
pub mod identity;
#[cfg(test)]
mod layout_lifecycle_tests;
pub mod lifecycle;
pub mod lock;
pub mod placement;
pub mod repo_ops;
pub mod repo_workspace;
pub mod service;
pub mod session_options;
#[cfg(test)]
pub(crate) mod test_support;

pub use allocate::{BaselineSource, RecoveredWorkspace, WorkspaceAllocator, checkout_path_for};
pub use capture::{
    CaptureRetryPolicy, CapturedState, capture_working_state, capture_working_state_validated,
};
pub use domain::{
    Baseline, BaselineMode, DelegationIsolation, ExecutionMode, ManagedBranch, ProvisioningPhase,
    RepositoryIdentity, ResolvedIsolation, ResultProvenance, ResultVerificationStatus,
    WorkspaceCreationOptions, WorkspaceId, WorkspaceLifecycle, WorkspaceOwnership, WorkspaceRecord,
    baseline_ref_for, resolve_delegation_isolation,
};
pub use error::{WorktreeError, WorktreeResult};
pub use gate::CheckoutMutationGate;
#[cfg(test)]
pub(crate) use git::{CliGit, DEFAULT_GIT_TIMEOUT, GitOutput, GitRunner};
pub use identity::{
    CheckoutIdentity, ResolvedRepository, ensure_relative_cwd_exists, relative_cwd,
    resolve_repository,
};
pub use lifecycle::{RemovalReport, WorkspaceManager, WorkspaceStatus};
pub use lock::{admin_lock_path, with_admin_lock};
pub use repo_ops::{
    CapturedWorkingState, CheckoutStatus, FailurePoint, FaultInjectingRepoOps, GixRepoOps,
    RegisteredWorktree, RepositoryOps, validate_ref_name,
};
pub use repo_workspace::{SqliteWorkspaceRepository, WorkspaceRepository};
pub use service::{WorkspaceBinding, WorkspaceService};
pub use session_options::{
    SESSION_WORKSPACE_META_KEY, SessionCreationOptions, fork_creation_options_from_meta,
    session_creation_options_from_meta,
};
