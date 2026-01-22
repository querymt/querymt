//! Workspace index coordinator
//!
//! Provides a unified workspace indexing system that coordinates file watching
//! and function indexing for both UI autocomplete and code similarity detection.

use super::file_index::{
    FileChangeSet, FileIndex, FileIndexConfig, FileIndexError, FileIndexWatcher,
};
use super::function_index::{FunctionIndex, FunctionIndexConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::RwLock;

/// Error types for workspace index operations
#[derive(Debug, Error)]
pub enum WorkspaceIndexError {
    #[error("File index error: {0}")]
    FileIndex(#[from] FileIndexError),
    #[error("Function index error: {0}")]
    FunctionIndex(String),
}

/// Statistics about the workspace indexes
#[derive(Debug, Clone)]
pub struct WorkspaceIndexStats {
    /// Total number of files in the file index
    pub file_count: usize,
    /// Total number of functions in the function index
    pub function_count: usize,
    /// Number of files with indexed functions
    pub indexed_file_count: usize,
    /// Age of the file index in seconds
    pub file_index_age_secs: Option<u64>,
}

/// Unified workspace index coordinating file watching and function indexing.
///
/// This is the main entry point for workspace indexing. It:
/// - Watches for file system changes
/// - Maintains a file listing for UI autocomplete
/// - Keeps the function index up-to-date incrementally
pub struct WorkspaceIndex {
    root: PathBuf,
    file_watcher: Arc<FileIndexWatcher>,
    function_index: Arc<RwLock<FunctionIndex>>,
    _update_handle: tokio::task::JoinHandle<()>,
}

impl WorkspaceIndex {
    /// Create a new workspace index for the given root directory.
    pub async fn new(root: PathBuf) -> Result<Self, WorkspaceIndexError> {
        Self::with_config(
            root,
            FileIndexConfig::default(),
            FunctionIndexConfig::default(),
        )
        .await
    }

    /// Create with custom configuration.
    pub async fn with_config(
        root: PathBuf,
        file_config: FileIndexConfig,
        function_config: FunctionIndexConfig,
    ) -> Result<Self, WorkspaceIndexError> {
        // Create file watcher
        let file_watcher =
            Arc::new(FileIndexWatcher::with_config(root.clone(), file_config).await?);

        // Build initial function index
        let function_index = FunctionIndex::build(&root, function_config.clone())
            .await
            .map_err(WorkspaceIndexError::FunctionIndex)?;
        let function_index = Arc::new(RwLock::new(function_index));

        // Subscribe to file changes for incremental function index updates
        let mut changes_rx = file_watcher.subscribe_changes();
        let function_index_clone = function_index.clone();
        let root_clone = root.clone();

        let update_handle = tokio::spawn(async move {
            while let Ok(change_set) = changes_rx.recv().await {
                if change_set.has_source_changes() {
                    update_function_index(&function_index_clone, &change_set, &root_clone).await;
                }
            }
        });

        Ok(Self {
            root,
            file_watcher,
            function_index,
            _update_handle: update_handle,
        })
    }

    /// Get the file watcher (for UI file autocomplete).
    pub fn file_watcher(&self) -> &Arc<FileIndexWatcher> {
        &self.file_watcher
    }

    /// Get the current file index.
    pub fn file_index(&self) -> Option<FileIndex> {
        self.file_watcher.get_index()
    }

    /// Get read access to the function index.
    pub async fn function_index(&self) -> tokio::sync::RwLockReadGuard<'_, FunctionIndex> {
        self.function_index.read().await
    }

    /// Get a clone of the function index handle.
    pub fn function_index_handle(&self) -> Arc<RwLock<FunctionIndex>> {
        self.function_index.clone()
    }

    /// Force a full rebuild of all indexes.
    pub async fn rebuild(&self) -> Result<(), WorkspaceIndexError> {
        // Rebuild file index
        self.file_watcher.refresh().await?;

        // Rebuild function index
        let new_function_index = FunctionIndex::build(&self.root, FunctionIndexConfig::default())
            .await
            .map_err(WorkspaceIndexError::FunctionIndex)?;

        *self.function_index.write().await = new_function_index;

        Ok(())
    }

    /// Get statistics about the indexes.
    pub async fn stats(&self) -> WorkspaceIndexStats {
        let file_index = self.file_watcher.get_index();
        let function_index = self.function_index.read().await;

        let file_count = file_index.as_ref().map(|i| i.files.len()).unwrap_or(0);
        let file_index_age_secs = file_index.as_ref().map(|i| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().saturating_sub(i.generated_at))
                .unwrap_or(0)
        });

        WorkspaceIndexStats {
            file_count,
            function_count: function_index.function_count(),
            indexed_file_count: function_index.file_count(),
            file_index_age_secs,
        }
    }

    /// Get the root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Update function index based on file changes
async fn update_function_index(
    function_index: &Arc<RwLock<FunctionIndex>>,
    change_set: &FileChangeSet,
    _root: &Path,
) {
    let mut index = function_index.write().await;

    // Remove deleted files
    for path in change_set.files_to_remove() {
        index.remove_file(path);
        log::debug!("FunctionIndex: Removed {:?}", path);
    }

    // Reindex created/modified files
    for path in change_set.files_to_reindex() {
        if let Ok(content) = std::fs::read_to_string(path) {
            index.update_file(path, &content);
            log::debug!("FunctionIndex: Updated {:?}", path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_workspace_index_creation() {
        let temp_dir = TempDir::new().unwrap();

        // Create some test files
        fs::write(temp_dir.path().join("test.txt"), "content").unwrap();
        fs::write(
            temp_dir.path().join("test.ts"),
            r#"
function hello() {
    console.log("hello");
    return "world";
}
"#,
        )
        .unwrap();

        let workspace = WorkspaceIndex::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();

        let stats = workspace.stats().await;
        assert!(stats.file_count >= 2);
    }

    #[tokio::test]
    async fn test_workspace_index_stats() {
        let temp_dir = TempDir::new().unwrap();

        fs::write(temp_dir.path().join("file.txt"), "content").unwrap();

        let workspace = WorkspaceIndex::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();

        let stats = workspace.stats().await;
        assert!(stats.file_count >= 1);
        assert!(stats.file_index_age_secs.is_some());
    }
}
