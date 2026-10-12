//! Error types for managed worktree workspaces.

use thiserror::Error;

/// Errors raised while resolving, provisioning, or administrating managed
/// worktree workspaces.
#[derive(Debug, Error)]
pub enum WorktreeError {
    /// The working directory is not inside a Git repository.
    #[error("not a Git repository: {path}")]
    NotGitRepository { path: std::path::PathBuf },

    /// Bare repositories are unsupported for workspace isolation in this
    /// version (initial support targets ordinary non-bare repositories and
    /// linked worktrees).
    #[error("bare repositories are unsupported for workspace isolation: {path}")]
    BareRepositoryUnsupported { path: std::path::PathBuf },

    /// The session cwd is not inside the resolved checkout root.
    #[error("session cwd {cwd} is outside checkout root {root}")]
    RelativeCwdOutsideCheckout {
        cwd: std::path::PathBuf,
        root: std::path::PathBuf,
    },

    /// The session's relative cwd does not exist in the target checkout
    /// (e.g. a baseline commit predating the subdirectory).
    #[error("relative cwd {relative:?} does not exist in checkout {root}")]
    RelativeCwdAbsent {
        relative: std::path::PathBuf,
        root: std::path::PathBuf,
    },

    /// Filesystem access failed while resolving paths.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    /// Git repository discovery failed for a reason other than "not found".
    #[error("git discovery failed: {0}")]
    GitDiscovery(String),

    /// A Git CLI command exited non-zero.
    #[error("git command failed ({status}): {argv}\n{stderr}")]
    GitCommand {
        /// Full argv as executed (without the binary name).
        argv: String,
        /// Exit status description, when the process exited normally.
        status: String,
        /// Captured stderr (lossy UTF-8), truncated for display.
        stderr: String,
    },

    /// The Git CLI command timed out and was killed.
    #[error("git command timed out after {timeout_secs}s: {argv}")]
    GitTimeout {
        /// Full argv as executed (without the binary name).
        argv: String,
        timeout_secs: u64,
    },

    /// The Git CLI process could not be spawned or waited on.
    #[error("failed to run git command '{argv}': {source}")]
    GitSpawn {
        /// Full argv as executed (without the binary name).
        argv: String,
        #[source]
        source: std::io::Error,
    },

    /// Durable workspace storage (registry) access failed.
    #[error("workspace storage error: {0}")]
    Storage(#[from] crate::session::error::SessionError),

    /// A workspace operation was invalid in the current state.
    #[error("invalid workspace operation: {0}")]
    InvalidOperation(String),

    /// A session bound to a managed workspace was loaded or resumed with a
    /// conflicting cwd request. Load/resume must use the persisted bound
    /// cwd; conflicting requests fail closed instead of falling back.
    #[error(
        "session {session_id} is bound to workspace {workspace_id} at {bound}; \
         refusing conflicting cwd {requested}"
    )]
    SessionCwdConflict {
        session_id: String,
        workspace_id: String,
        bound: std::path::PathBuf,
        requested: std::path::PathBuf,
    },

    /// A session's bound workspace checkout is missing on disk. Attachment
    /// fails closed; there is no fallback to the original checkout.
    #[error(
        "workspace {workspace_id} checkout {checkout} is missing; the session \
         cannot attach (re-create the workspace instead of falling back)"
    )]
    CheckoutMissing {
        workspace_id: String,
        checkout: std::path::PathBuf,
    },
}

/// Convenience alias for [`WorktreeError`] results.
pub type WorktreeResult<T> = Result<T, WorktreeError>;
