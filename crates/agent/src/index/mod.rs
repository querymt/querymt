pub mod file_index;
pub mod function_index;
pub mod merkle;
pub mod outline_index;
pub mod search;
pub mod symbol_index;
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
pub use symbol_index::{SymbolEntry, SymbolError, SymbolIndex, SymbolKind, parse_kind_filter};
pub use workspace_actor::{
    WorkspaceHandle, WorkspaceIndexActor, WorkspaceIndexError, WorkspaceIndexStats,
};
pub use workspace_manager_actor::{
    GetOrCreate, WorkspaceIndexManagerActor, WorkspaceIndexManagerConfig,
};
pub use workspace_root::{normalize_cwd, resolve_workspace_root};
