//! Trait definition for snapshot backends

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur during snapshot operations.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// The underlying git/VCS repository could not be opened, initialised, or
    /// written to.
    #[error("Snapshot repository error: {0}")]
    Repository(String),

    /// A snapshot ID (commit SHA) supplied by the caller is not valid.
    #[error("Invalid snapshot ID: {0}")]
    InvalidSnapshotId(String),

    /// The requested snapshot could not be found in the store.
    #[error("Snapshot not found: {0}")]
    NotFound(String),

    /// A filesystem operation (read, write, mkdir, …) failed.
    #[error("Filesystem error: {0}")]
    Filesystem(String),

    /// The background `spawn_blocking` task panicked.
    #[error("Snapshot task panicked")]
    TaskPanicked,

    /// Catch-all for errors that don't fit the above categories.
    #[error("{0}")]
    Other(String),
}

/// Convenience `Result` alias for snapshot operations.
pub type SnapshotResult<T> = Result<T, SnapshotError>;

/// Unique identifier for a snapshot (e.g., git commit SHA)
pub type SnapshotId = String;

/// Configuration for garbage collection of old snapshots
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// Maximum number of snapshots to keep (oldest are removed first)
    pub max_snapshots: Option<usize>,
    /// Maximum age of snapshots in days (older are removed)
    pub max_age_days: Option<u64>,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            max_snapshots: Some(100),
            max_age_days: Some(30),
        }
    }
}

/// Result of a garbage collection operation
#[derive(Debug, Clone)]
pub struct GcResult {
    pub removed_count: usize,
    pub remaining_count: usize,
}

/// Trait for snapshot backend implementations
///
/// A snapshot backend tracks filesystem state and enables undo/redo operations
/// by creating snapshots, computing diffs, and restoring files.
#[async_trait]
pub trait SnapshotBackend: Send + Sync {
    /// Check if this backend can operate on the given worktree
    ///
    /// Returns true if the backend is properly configured and the worktree
    /// is accessible.
    fn is_available(&self, worktree: &Path) -> bool;

    /// Create a snapshot of the current worktree state
    ///
    /// Returns a unique snapshot ID (e.g., git commit SHA) that can be used
    /// to reference this snapshot later.
    ///
    /// # Arguments
    /// * `worktree` - The root directory to snapshot
    async fn track(&self, worktree: &Path) -> SnapshotResult<SnapshotId>;

    /// Compute which files changed between two snapshots
    ///
    /// # Arguments
    /// * `worktree` - The root directory
    /// * `pre` - Snapshot ID before changes
    /// * `post` - Snapshot ID after changes
    ///
    /// # Returns
    /// List of file paths (relative to worktree) that were added, modified, or removed
    async fn diff(
        &self,
        worktree: &Path,
        pre: &SnapshotId,
        post: &SnapshotId,
    ) -> SnapshotResult<Vec<PathBuf>>;

    /// Restore specific files from a snapshot
    ///
    /// # Arguments
    /// * `worktree` - The root directory
    /// * `snapshot` - Snapshot ID to restore from
    /// * `paths` - List of file paths to restore (relative to worktree)
    async fn restore_paths(
        &self,
        worktree: &Path,
        snapshot: &SnapshotId,
        paths: &[PathBuf],
    ) -> SnapshotResult<()>;

    /// Restore entire worktree to a snapshot
    ///
    /// # Arguments
    /// * `worktree` - The root directory
    /// * `snapshot` - Snapshot ID to restore to
    async fn restore(&self, worktree: &Path, snapshot: &SnapshotId) -> SnapshotResult<()>;

    /// Run garbage collection to prune old snapshots
    ///
    /// # Arguments
    /// * `worktree` - The root directory
    /// * `config` - GC configuration (max snapshots, max age)
    ///
    /// # Returns
    /// Statistics about snapshots removed and remaining
    async fn gc(&self, worktree: &Path, config: &GcConfig) -> SnapshotResult<GcResult>;
}
