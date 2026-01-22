pub mod file_index;
pub mod function_index;
pub mod merkle;
pub mod search;
pub mod workspace;
pub mod workspace_manager;
pub mod workspace_root;

// Re-export commonly used types
pub use file_index::{
    FileChangeKind, FileChangeSet, FileIndex, FileIndexConfig, FileIndexEntry, FileIndexError,
    FileIndexWatcher,
};
pub use function_index::{
    FunctionIndex, FunctionIndexConfig, IndexedFunctionEntry, SimilarFunctionMatch,
};
pub use merkle::DiffPaths;
pub use workspace::{WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexStats};
pub use workspace_manager::{WorkspaceIndexManager, WorkspaceIndexManagerConfig};
pub use workspace_root::{normalize_cwd, resolve_workspace_root};
