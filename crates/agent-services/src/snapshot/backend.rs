//! Trait definition for snapshot backends

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
    InvalidSnapshotId(SnapshotId),

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

pub use crate::snapshot::snapshot_backend::SnapshotBackend;
pub use querymt_agent_core::snapshot_id::SnapshotId;

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

impl From<&querymt_agent_core::config::execution::SnapshotBackendConfig> for GcConfig {
    fn from(config: &querymt_agent_core::config::execution::SnapshotBackendConfig) -> Self {
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
