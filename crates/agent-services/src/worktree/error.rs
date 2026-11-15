//! Error types for managed worktree workspaces.

use thiserror::Error;

/// Most paths named in a refusal diagnostic; the rest are only counted.
const PATH_SAMPLE_LIMIT: usize = 10;

/// A bounded sample of paths plus the total count, so diagnostics stay small
/// for checkouts with thousands of changed files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathSample {
    /// Boxed slice: keeps `WorktreeError` small (it is returned everywhere).
    pub paths: Box<[String]>,
    pub total: usize,
}

impl PathSample {
    pub fn new(all: &[String]) -> Self {
        Self {
            paths: all.iter().take(PATH_SAMPLE_LIMIT).cloned().collect(),
            total: all.len(),
        }
    }
}

impl std::fmt::Display for PathSample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.total)?;
        if self.total > 0 {
            write!(f, " ({}", self.paths.join(", "))?;
            if self.total > self.paths.len() {
                write!(f, ", +{} more", self.total - self.paths.len())?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

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

    /// The checkout still has an executing owner, shared delegates, or
    /// activity in another process.
    #[error("{0}")]
    OwnerActive(String),

    /// Removal needs the workspace archived first because a session owns it.
    #[error(
        "workspace {workspace_id} is active (owned by session {owner}); archive it first \
         (`/worktree archive`, or `/worktree remove --archive`) before removal"
    )]
    WorkspaceNotArchived { workspace_id: String, owner: String },

    /// Removal would discard working files that no ref preserves.
    #[error(
        "workspace {workspace_id} has unpreserved working state: uncommitted {uncommitted}, \
         untracked {untracked}, ignored {ignored}; commit or delete the listed files \
         before removal"
    )]
    UnpreservedContent {
        workspace_id: String,
        uncommitted: PathSample,
        untracked: PathSample,
        ignored: PathSample,
    },

    /// Removal would discard commits beyond the baseline that no retained ref
    /// contains.
    #[error(
        "workspace {workspace_id} has commits beyond its baseline without a retained \
         recovery reference: {commits}; publish the result or integrate the commits \
         before removal"
    )]
    UnpreservedCommits {
        workspace_id: String,
        commits: PathSample,
    },

    /// The workspace is archived, so the requested operation is refused.
    #[error(
        "workspace {workspace_id} is archived; it can only be inspected or removed \
         (`/worktree remove`)"
    )]
    WorkspaceArchived { workspace_id: String },
}

impl WorktreeError {
    /// Stable machine-readable code, identical on every surface (ACP error
    /// data, native command responses, simple API, Python). Human-readable
    /// messages may change; codes do not.
    pub fn code(&self) -> &'static str {
        use crate::session::error::SessionError;
        match self {
            Self::NotGitRepository { .. }
            | Self::BareRepositoryUnsupported { .. }
            | Self::RelativeCwdOutsideCheckout { .. }
            | Self::RelativeCwdAbsent { .. }
            | Self::GitDiscovery(_) => "isolation_unsupported_layout",
            Self::SessionCwdConflict { .. } => "conflicting_cwd",
            Self::CheckoutMissing { .. } => "workspace_missing",
            Self::Storage(SessionError::WorkspaceAlreadyOwned { .. }) => "owner_conflict",
            Self::Storage(SessionError::WorkspaceNotFound(_)) => "workspace_not_found",
            Self::Storage(_) => "storage_error",
            Self::Io(_)
            | Self::GitCommand { .. }
            | Self::GitTimeout { .. }
            | Self::GitSpawn { .. } => "repository_error",
            Self::InvalidOperation(_) => "operation_refused",
            Self::OwnerActive(_) => "owner_active",
            Self::WorkspaceNotArchived { .. } => "workspace_not_archived",
            Self::UnpreservedContent { .. } => "unpreserved_content",
            Self::UnpreservedCommits { .. } => "unpreserved_commits",
            Self::WorkspaceArchived { .. } => "workspace_archived",
        }
    }

    /// Whether this is one of the specific, actionable removal/archive
    /// refusals whose code replaces the generic `removal_refused`.
    pub fn is_typed_refusal(&self) -> bool {
        matches!(
            self,
            Self::OwnerActive(_)
                | Self::WorkspaceNotArchived { .. }
                | Self::UnpreservedContent { .. }
                | Self::UnpreservedCommits { .. }
                | Self::WorkspaceArchived { .. }
        )
    }
}

/// Convenience alias for [`WorktreeError`] results.
pub type WorktreeResult<T> = Result<T, WorktreeError>;
