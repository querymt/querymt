pub mod file_index;
pub mod function_index;
pub mod merkle;
pub mod search;
pub mod workspace_actor;
pub mod workspace_manager_actor;
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
pub use workspace_actor::{
    WorkspaceHandle, WorkspaceIndexActor, WorkspaceIndexError, WorkspaceIndexStats,
};
pub use workspace_manager_actor::{
    GetOrCreate, WorkspaceIndexManagerActor, WorkspaceIndexManagerConfig,
};
pub use workspace_root::{normalize_cwd, resolve_workspace_root};
