//! The `SnapshotBackend` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::backend::*;
use async_trait::async_trait;
use querymt_agent_core::snapshot_id::SnapshotId;
use std::path::{Path, PathBuf};

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

    /// Let the snapshot store of `worktree` read the baseline objects of its
    /// source repository, so a snapshot need not copy them.
    ///
    /// `objects_dir` is the source repository's object directory and
    /// `baseline_commit` the retained baseline commit of the managed workspace.
    /// Only objects reachable from that commit are trusted. The default does
    /// nothing: the store stays self-contained.
    ///
    /// # Errors
    /// Fails when the store cannot be opened or written, or `baseline_commit`
    /// is not an object id. A source with another object format is not an
    /// error: the store is left unlinked.
    async fn link_object_source(
        &self,
        _worktree: &Path,
        _objects_dir: &Path,
        _baseline_commit: &str,
    ) -> SnapshotResult<()> {
        Ok(())
    }
}
