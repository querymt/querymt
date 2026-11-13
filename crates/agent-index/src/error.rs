//! Typed errors for the workspace index.

use super::file_index::FileIndexError;
use thiserror::Error;

/// Failure building, refreshing or searching a workspace index.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexError {
    /// The file index could not be built or refreshed.
    #[error("file index error: {0}")]
    FileIndex(#[from] FileIndexError),
    /// A search pattern is not a valid regex.
    #[error("invalid search pattern: {0}")]
    Regex(#[from] grep_regex::Error),
    /// Reading a file during a search failed.
    #[error("search failed: {0}")]
    Search(#[source] std::io::Error),
    /// The blocking index build task panicked or was cancelled.
    #[error("index build task failed: {0}")]
    TaskFailed(#[from] tokio::task::JoinError),
    /// The parser thread pool could not be created.
    #[error("failed to build index thread pool: {0}")]
    ThreadPool(#[from] rayon::ThreadPoolBuildError),
}
