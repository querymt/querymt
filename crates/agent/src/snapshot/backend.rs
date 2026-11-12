//! Trait definition for snapshot backends

use async_trait::async_trait;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur during snapshot operations.
///
/// Callers that flatten errors into strings (`UndoError` does) only keep the
/// `Display` output, so [`SnapshotError::Io`] includes the OS error in its
/// message. It is also exposed through `source()`; reporters that print the
/// whole chain will therefore show the OS error twice, which is the price of
/// not losing it when the error is flattened.
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// An I/O operation on a named path failed. Keeps the OS error as the
    /// error source so callers can inspect its kind.
    #[error("{op} {path}: {source}", path = path.display())]
    Io {
        /// What was being attempted, e.g. `"write"` or `"delete"`.
        op: &'static str,
        /// The path the operation was applied to.
        path: PathBuf,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The background `spawn_blocking` task panicked.
    #[error("Snapshot task panicked")]
    TaskPanicked,

    /// Catch-all for errors that don't fit the above categories.
    #[error("{0}")]
    Other(String),
}

/// Build a [`SnapshotError::Io`] mapper for `op` applied to `path`.
///
/// The path is only borrowed here and copied when an error actually happens,
/// so building the mapper on a success path costs nothing.
///
/// ```ignore
/// fs::remove_file(&target).map_err(io_err("delete", &target))?;
/// ```
pub(crate) fn io_err<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(std::io::Error) -> SnapshotError + 'a {
    move |source| SnapshotError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

/// Convenience `Result` alias for snapshot operations.
pub type SnapshotResult<T> = Result<T, SnapshotError>;

/// Unique identifier for a snapshot (e.g., git commit SHA)
pub type SnapshotId = String;

/// Configuration for garbage collection of old snapshots.
///
/// The default has no limits, which disables garbage collection: nothing is
/// ever deleted unless a limit is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcConfig {
    /// Maximum number of snapshots to keep (oldest are removed first)
    pub max_snapshots: Option<NonZeroUsize>,
    /// Maximum age of snapshots in days (older are removed)
    pub max_age_days: Option<NonZeroU64>,
}

impl GcConfig {
    /// `true` when at least one limit is set, i.e. GC may delete snapshots.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.max_snapshots.is_some() || self.max_age_days.is_some()
    }
}

impl From<&crate::config::SnapshotBackendConfig> for GcConfig {
    fn from(config: &crate::config::SnapshotBackendConfig) -> Self {
        Self {
            max_snapshots: config.max_snapshots,
            max_age_days: config.max_age_days,
        }
    }
}

/// Result of a garbage collection operation
///
/// The struct is `#[non_exhaustive]`: external backends build it from
/// [`GcResult::default`] and set the fields they know about, so fields added
/// later start at zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GcResult {
    /// Snapshots (commits) pruned.
    pub removed_count: usize,
    /// Snapshots still reachable from the latest one.
    pub remaining_count: usize,
    /// Loose objects (commits, trees, blobs) deleted from disk.
    pub deleted_objects: usize,
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
    /// # Errors
    /// Fails when the snapshot store cannot be opened or written, or a file
    /// in scope cannot be read.
    ///
    /// Returns a unique snapshot ID (e.g., git commit SHA) that can be used
    /// to reference this snapshot later.
    ///
    /// # Arguments
    /// * `worktree` - The root directory to snapshot
    async fn track(&self, worktree: &Path) -> SnapshotResult<SnapshotId>;

    /// Compute which files changed between two snapshots
    ///
    /// # Errors
    /// Fails when either snapshot id is invalid or missing from the store.
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
    /// Paths the snapshot holds are rewritten (atomically, keeping the
    /// existing permissions); paths it lacks are deleted.
    ///
    /// # Errors
    /// Fails on an unknown snapshot, a path that escapes `worktree`, or the
    /// first write or delete that fails. Earlier paths stay restored.
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

    /// Restore several snapshots' paths as one unit, in order.
    ///
    /// Backends that can should keep every snapshot in `steps` alive (and
    /// resolved before the first file changes) for the whole operation. The
    /// default restores step by step with [`restore_paths`](Self::restore_paths).
    ///
    /// # Errors
    /// As [`restore_paths`](Self::restore_paths); earlier steps stay restored.
    async fn restore_batch(
        &self,
        worktree: &Path,
        steps: &[(SnapshotId, Vec<PathBuf>)],
    ) -> SnapshotResult<()> {
        for (snapshot, paths) in steps {
            self.restore_paths(worktree, snapshot, paths).await?;
        }
        Ok(())
    }

    /// Paths among `paths` that exist as files in the worktree but are missing
    /// from `snapshot`, so overwriting or deleting them could not be undone
    /// from that snapshot (for example after an ignore rule was added).
    ///
    /// The default reports none: a backend that cannot tell does not block.
    ///
    /// # Errors
    /// Fails when `snapshot` is invalid or missing, or a path escapes `worktree`.
    async fn uncaptured_paths(
        &self,
        _worktree: &Path,
        _snapshot: &SnapshotId,
        _paths: &[PathBuf],
    ) -> SnapshotResult<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    /// Restore entire worktree to a snapshot
    ///
    /// # Errors
    /// Fails on an unknown snapshot or on the first file that cannot be
    /// written; earlier files stay restored.
    ///
    /// # Arguments
    /// * `worktree` - The root directory
    /// * `snapshot` - Snapshot ID to restore to
    async fn restore(&self, worktree: &Path, snapshot: &SnapshotId) -> SnapshotResult<()>;

    /// Run garbage collection to prune old snapshots
    ///
    /// # Errors
    /// Fails when the snapshot store cannot be read or an object cannot be
    /// deleted. With no limit set in `config` nothing is deleted.
    ///
    /// # Arguments
    /// * `worktree` - The root directory
    /// * `config` - GC configuration (max snapshots, max age)
    ///
    /// # Returns
    /// Statistics about snapshots removed and remaining
    async fn gc(&self, worktree: &Path, config: &GcConfig) -> SnapshotResult<GcResult>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn io_error_names_operation_and_path() {
        let cause = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let err = io_err("delete", Path::new("/work/sub/new.txt"))(cause);

        let shown = err.to_string();
        assert!(shown.contains("delete"), "{shown}");
        assert!(shown.contains("/work/sub/new.txt"), "{shown}");
        let source = err.source().expect("io error is the source");
        let io = source.downcast_ref::<std::io::Error>().expect("io::Error");
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
