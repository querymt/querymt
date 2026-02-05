//! File index for workspace file listing and watching
//!
//! Provides real-time file indexing with file system watching for UI autocomplete
//! and incremental function index updates.

use arc_swap::ArcSwap;
use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};
use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};

/// Error types for file index operations
#[derive(Debug, Error)]
pub enum FileIndexError {
    #[error("Failed to create file watcher: {0}")]
    WatcherCreation(#[from] notify::Error),
    #[error("Failed to walk directory: {0}")]
    WalkError(String),
    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Invalid config: {0}")]
    InvalidConfig(String),
}

/// A single entry in the file index.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FileIndexEntry {
    /// Relative path from the index root
    pub path: String,
    /// Whether this entry is a directory
    pub is_dir: bool,
}

/// A snapshot of the file index at a point in time.
#[derive(Debug, Clone)]
pub struct FileIndex {
    /// All indexed files and directories
    pub files: Vec<FileIndexEntry>,
    /// Unix timestamp when this index was generated
    pub generated_at: u64,
    /// Root directory this index was built from
    pub root: PathBuf,
}

/// Type of file change detected
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChangeKind {
    Created,
    Modified,
    Removed,
    Renamed { from: PathBuf },
}

/// A file change event with full context
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: FileChangeKind,
    pub is_dir: bool,
}

/// Detailed change set from a batch of file system events
#[derive(Debug, Clone, Default)]
pub struct FileChangeSet {
    pub changes: Vec<FileChange>,
    pub timestamp: u64,
}

impl FileChangeSet {
    /// Get all files that were created or modified (need reindexing)
    pub fn files_to_reindex(&self) -> impl Iterator<Item = &PathBuf> {
        self.changes.iter().filter_map(|c| {
            if !c.is_dir && matches!(c.kind, FileChangeKind::Created | FileChangeKind::Modified) {
                Some(&c.path)
            } else {
                None
            }
        })
    }

    /// Get all files that were removed (need removal from index)
    pub fn files_to_remove(&self) -> impl Iterator<Item = &PathBuf> {
        self.changes.iter().filter_map(|c| {
            if !c.is_dir && matches!(c.kind, FileChangeKind::Removed) {
                Some(&c.path)
            } else {
                None
            }
        })
    }

    /// Check if any source files changed (for FunctionIndex)
    pub fn has_source_changes(&self) -> bool {
        self.changes
            .iter()
            .any(|c| !c.is_dir && is_source_file(&c.path))
    }
}

/// Check if a path is a source file that should be indexed
pub fn is_source_file(path: &Path) -> bool {
    let supported_extensions = [
        "ts", "tsx", "js", "jsx", "mjs", "cjs",  // TypeScript/JavaScript
        "rs",   // Rust
        "go",   // Go
        "java", // Java
        "c", "h", "cpp", "hpp", "cc", "cxx", // C/C++
        "cs",  // C#
        "rb",  // Ruby
    ];

    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| supported_extensions.contains(&ext))
        .unwrap_or(false)
}

/// Configuration for the file index watcher.
#[derive(Debug, Clone)]
pub struct FileIndexConfig {
    /// Maximum number of files to index
    pub max_files: usize,
    /// Maximum directory depth to traverse
    pub max_depth: Option<usize>,
    /// Whether to include hidden files (dotfiles)
    pub include_hidden: bool,
    /// Whether to respect .gitignore files
    pub respect_gitignore: bool,
    /// Additional ignore patterns for file indexing
    pub ignore_patterns: Vec<String>,
    /// Debounce duration for file system events
    pub debounce: Duration,
    /// Minimum interval between index rebuilds
    pub rebuild_interval: Duration,
    /// Interval for periodic full rebuilds
    pub full_rebuild_interval: Duration,
}

impl Default for FileIndexConfig {
    fn default() -> Self {
        Self {
            max_files: 10_000,
            max_depth: Some(20),
            include_hidden: true, // Include dotfiles like .github, .eslintrc, etc.
            respect_gitignore: true,
            ignore_patterns: vec![
                // Version control
                ".git/".to_string(),
                // Dependencies
                "node_modules/".to_string(),
                "vendor/".to_string(),
                // Build outputs
                "dist/".to_string(),
                "build/".to_string(),
                "out/".to_string(),
                "target/".to_string(),
                ".next/".to_string(),
                ".nuxt/".to_string(),
                // Python
                "__pycache__/".to_string(),
                "*.pyc".to_string(),
                ".venv/".to_string(),
                "venv/".to_string(),
                // Caches and temp
                ".cache/".to_string(),
                "coverage/".to_string(),
                "tmp/".to_string(),
                "temp/".to_string(),
                // Databases
                "*.db".to_string(),
                "*.sqlite".to_string(),
                "*.sqlite3".to_string(),
                // IDE and editor files (exclude these dotfiles)
                ".vscode/".to_string(),
                ".idea/".to_string(),
                ".DS_Store".to_string(),
                // Environment files (sensitive)
                ".env".to_string(),
                ".env.*".to_string(),
                // QueryMT agent files
                ".opencode/".to_string(),
            ],
            debounce: Duration::from_millis(200),
            rebuild_interval: Duration::from_millis(1000),
            full_rebuild_interval: Duration::from_secs(15 * 60),
        }
    }
}

/// A file system index with automatic updates via file watching.
pub struct FileIndexWatcher {
    index: Arc<ArcSwap<Option<FileIndex>>>,
    root: PathBuf,
    config: FileIndexConfig,

    // Broadcast the full index (for UI file listing)
    index_tx: broadcast::Sender<FileIndex>,

    // Broadcast granular changes (for FunctionIndex)
    changes_tx: broadcast::Sender<FileChangeSet>,

    // Keep watcher alive
    _watcher: RecommendedWatcher,

    // Keep rebuild task alive
    _rebuild_handle: tokio::task::JoinHandle<()>,
}

impl FileIndexWatcher {
    /// Create a new file index watcher with default configuration.
    pub async fn new(root: PathBuf) -> Result<Self, FileIndexError> {
        Self::with_config(root, FileIndexConfig::default()).await
    }

    /// Create a new file index watcher with custom configuration.
    pub async fn with_config(
        root: PathBuf,
        config: FileIndexConfig,
    ) -> Result<Self, FileIndexError> {
        let index = Arc::new(ArcSwap::from_pointee(None));
        let (index_tx, _) = broadcast::channel(16);
        let (changes_tx, _) = broadcast::channel(64);
        let (event_tx, mut event_rx) = mpsc::channel::<Event>(256);
        let overrides = compile_overrides(&root, &config)?;

        // Create the file watcher
        let watcher_config = Config::default().with_poll_interval(Duration::from_secs(2));

        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = event_tx.blocking_send(event);
                }
            },
            watcher_config,
        )?;

        // Start watching the root directory
        watcher.watch(&root, RecursiveMode::Recursive)?;

        // Build initial index using spawn_blocking to avoid blocking the async runtime
        let root_clone = root.clone();
        let config_clone = config.clone();
        let overrides_clone = overrides.clone();
        let initial_index = tokio::task::spawn_blocking(move || {
            build_file_index(&root_clone, &config_clone, &overrides_clone)
        })
        .await
        .map_err(|e| {
            FileIndexError::WalkError(format!("Failed to build initial index: {}", e))
        })??;

        index.store(Arc::new(Some(initial_index.clone())));
        let _ = index_tx.send(initial_index);

        // Spawn the event processing task
        let index_clone = index.clone();
        let index_tx_clone = index_tx.clone();
        let changes_tx_clone = changes_tx.clone();
        let root_clone = root.clone();
        let config_clone = config.clone();
        let overrides_clone = overrides.clone();

        let rebuild_handle = tokio::spawn(async move {
            let mut pending_events: Vec<Event> = Vec::new();
            let mut last_rebuild = std::time::Instant::now();
            let start = tokio::time::Instant::now() + config_clone.full_rebuild_interval;
            let mut full_rebuild_timer =
                tokio::time::interval_at(start, config_clone.full_rebuild_interval);
            let debounce_duration = config_clone.debounce;
            let rebuild_interval = config_clone.rebuild_interval;

            loop {
                tokio::select! {
                    biased;
                    _ = full_rebuild_timer.tick() => {
                        let root = root_clone.clone();
                        let config = config_clone.clone();
                        let overrides = overrides_clone.clone();

                        if let Ok(Ok(new_index)) = tokio::task::spawn_blocking(move || {
                            build_file_index(&root, &config, &overrides)
                        }).await {
                            index_clone.store(Arc::new(Some(new_index.clone())));
                            let _ = index_tx_clone.send(new_index);
                        }
                    }
                    result = event_rx.recv() => {
                        match result {
                            Some(event) => pending_events.push(event),
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep(debounce_duration) => {
                        if !pending_events.is_empty() && last_rebuild.elapsed() >= rebuild_interval {
                            let change_set = process_events(
                                &pending_events,
                                &root_clone,
                                &overrides_clone,
                            );
                            pending_events.clear();

                            if !change_set.changes.is_empty() {
                                let _ = changes_tx_clone.send(change_set.clone());
                                if let Some(current) = (**index_clone.load()).clone() {
                                    let updated = apply_changes_to_index(
                                        &current,
                                        &change_set,
                                        &root_clone,
                                        &config_clone,
                                        &overrides_clone,
                                    );
                                    index_clone.store(Arc::new(Some(updated.clone())));
                                    let _ = index_tx_clone.send(updated);
                                } else {
                                    // No current index, do full rebuild in spawn_blocking
                                    let root = root_clone.clone();
                                    let config = config_clone.clone();
                                    let overrides = overrides_clone.clone();

                                    if let Ok(Ok(new_index)) = tokio::task::spawn_blocking(move || {
                                        build_file_index(&root, &config, &overrides)
                                    }).await {
                                        index_clone.store(Arc::new(Some(new_index.clone())));
                                        let _ = index_tx_clone.send(new_index);
                                    }
                                }

                                last_rebuild = std::time::Instant::now();
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            index,
            root,
            config,
            index_tx,
            changes_tx,
            _watcher: watcher,
            _rebuild_handle: rebuild_handle,
        })
    }

    /// Get the current file index, if available.
    /// This is a lock-free operation that never blocks.
    pub fn get_index(&self) -> Option<FileIndex> {
        (**self.index.load()).clone()
    }

    /// Subscribe to full index updates (for UI autocomplete).
    pub fn subscribe_index(&self) -> broadcast::Receiver<FileIndex> {
        self.index_tx.subscribe()
    }

    /// Subscribe to granular file changes (for FunctionIndex).
    pub fn subscribe_changes(&self) -> broadcast::Receiver<FileChangeSet> {
        self.changes_tx.subscribe()
    }

    /// Force an immediate index rebuild.
    pub async fn refresh(&self) -> Result<FileIndex, FileIndexError> {
        let overrides = compile_overrides(&self.root, &self.config)?;
        let root = self.root.clone();
        let config = self.config.clone();

        let new_index =
            tokio::task::spawn_blocking(move || build_file_index(&root, &config, &overrides))
                .await
                .map_err(|e| {
                    FileIndexError::WalkError(format!("Failed to refresh index: {}", e))
                })??;

        self.index.store(Arc::new(Some(new_index.clone())));
        let _ = self.index_tx.send(new_index.clone());
        Ok(new_index)
    }

    /// Get the root directory being watched.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }
}

/// Build a file index from a directory
fn build_file_index(
    root: &Path,
    config: &FileIndexConfig,
    overrides: &Override,
) -> Result<FileIndex, FileIndexError> {
    let mut files = Vec::new();
    let mut count = 0;

    let mut builder = WalkBuilder::new(root);
    builder
        .git_ignore(config.respect_gitignore)
        .hidden(!config.include_hidden)
        .follow_links(false)
        .overrides(overrides.clone());

    if let Some(max_depth) = config.max_depth {
        builder.max_depth(Some(max_depth));
    }

    for entry in builder.build().filter_map(|e| e.ok()) {
        if count >= config.max_files {
            log::warn!("File index reached max_files limit of {}", config.max_files);
            break;
        }

        let path = entry.path();

        // Skip the root directory itself
        if path == root {
            continue;
        }

        // Get relative path
        let relative_path = match path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // Skip empty paths
        if relative_path.is_empty() {
            continue;
        }

        let is_dir = path.is_dir();

        files.push(FileIndexEntry {
            path: relative_path,
            is_dir,
        });

        count += 1;
    }

    // Sort files: directories first, then alphabetically
    files.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.path.cmp(&b.path),
    });

    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    log::info!(
        "FileIndex: Indexed {} files/directories in {:?}",
        files.len(),
        root
    );

    Ok(FileIndex {
        files,
        generated_at,
        root: root.to_path_buf(),
    })
}

/// Process notify events into a FileChangeSet
fn process_events(events: &[Event], root: &Path, overrides: &Override) -> FileChangeSet {
    let mut changes = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut rename_from: Option<PathBuf> = None;

    for event in events {
        for path in &event.paths {
            // Skip if we've already processed this path
            if seen_paths.contains(path) {
                continue;
            }

            let is_dir = path.is_dir();

            let relative = match path.strip_prefix(root) {
                Ok(rel) => rel.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if is_ignored(&relative, is_dir, overrides) {
                continue;
            }

            let change_kind = match &event.kind {
                EventKind::Create(CreateKind::File) | EventKind::Create(CreateKind::Folder) => {
                    Some(FileChangeKind::Created)
                }
                EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) => {
                    Some(FileChangeKind::Modified)
                }
                EventKind::Remove(RemoveKind::File) | EventKind::Remove(RemoveKind::Folder) => {
                    Some(FileChangeKind::Removed)
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    rename_from = Some(path.clone());
                    None
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    let from = rename_from.take();
                    Some(FileChangeKind::Renamed {
                        from: from.unwrap_or_default(),
                    })
                }
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                    // Both paths are in event.paths
                    Some(FileChangeKind::Modified)
                }
                _ => None,
            };

            if let Some(kind) = change_kind {
                // Check if path is within root
                if path.starts_with(root) {
                    seen_paths.insert(path.clone());
                    changes.push(FileChange {
                        path: path.clone(),
                        kind,
                        is_dir,
                    });
                }
            }
        }
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    FileChangeSet { changes, timestamp }
}

fn compile_overrides(root: &Path, config: &FileIndexConfig) -> Result<Override, FileIndexError> {
    let mut builder = OverrideBuilder::new(root);

    // Add each pattern with ! prefix (meaning "exclude" in Override semantics)
    for pattern in &config.ignore_patterns {
        let exclude_pattern = format!("!{}", pattern);
        builder.add(&exclude_pattern).map_err(|e| {
            FileIndexError::InvalidConfig(format!("Invalid pattern '{}': {}", pattern, e))
        })?;
    }

    builder
        .build()
        .map_err(|e| FileIndexError::InvalidConfig(format!("Failed to build overrides: {}", e)))
}

fn is_ignored(relative_path: &str, is_dir: bool, overrides: &Override) -> bool {
    overrides.matched(relative_path, is_dir).is_ignore()
}

fn apply_changes_to_index(
    current: &FileIndex,
    change_set: &FileChangeSet,
    root: &Path,
    config: &FileIndexConfig,
    overrides: &Override,
) -> FileIndex {
    let mut entries: HashMap<String, FileIndexEntry> = current
        .files
        .iter()
        .map(|entry| (entry.path.clone(), entry.clone()))
        .collect();

    for change in &change_set.changes {
        let relative_path = match change.path.strip_prefix(root) {
            Ok(path) => path.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        if relative_path.is_empty() || is_ignored(&relative_path, change.is_dir, overrides) {
            continue;
        }

        match &change.kind {
            FileChangeKind::Created => {
                entries.insert(
                    relative_path.clone(),
                    FileIndexEntry {
                        path: relative_path,
                        is_dir: change.is_dir,
                    },
                );
            }
            FileChangeKind::Removed => {
                entries.remove(&relative_path);
            }
            FileChangeKind::Renamed { from } => {
                if let Ok(from_rel) = from.strip_prefix(root) {
                    let from_rel = from_rel.to_string_lossy().to_string();
                    entries.remove(&from_rel);
                }
                entries.insert(
                    relative_path.clone(),
                    FileIndexEntry {
                        path: relative_path,
                        is_dir: change.is_dir,
                    },
                );
            }
            FileChangeKind::Modified => {
                if !entries.contains_key(&relative_path) {
                    entries.insert(
                        relative_path.clone(),
                        FileIndexEntry {
                            path: relative_path,
                            is_dir: change.is_dir,
                        },
                    );
                }
            }
        }
    }

    let mut files: Vec<FileIndexEntry> = entries.into_values().collect();
    files.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.path.cmp(&b.path),
    });

    if files.len() > config.max_files {
        files.truncate(config.max_files);
        log::warn!("File index reached max_files limit of {}", config.max_files);
    }

    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    FileIndex {
        files,
        generated_at,
        root: root.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_build_file_index() {
        let temp_dir = TempDir::new().unwrap();

        // Create some test files
        fs::write(temp_dir.path().join("file1.txt"), "content").unwrap();
        fs::write(temp_dir.path().join("file2.rs"), "fn main() {}").unwrap();
        fs::create_dir(temp_dir.path().join("subdir")).unwrap();
        fs::write(temp_dir.path().join("subdir/nested.ts"), "export {}").unwrap();

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(temp_dir.path(), &config).unwrap();
        let index = build_file_index(temp_dir.path(), &config, &overrides).unwrap();

        assert!(index.files.len() >= 3);
        assert!(index.files.iter().any(|f| f.path == "file1.txt"));
        assert!(index.files.iter().any(|f| f.path == "file2.rs"));
        assert!(index.files.iter().any(|f| f.path.contains("subdir")));
    }

    #[tokio::test]
    async fn test_file_index_watcher_creation() {
        let temp_dir = TempDir::new().unwrap();

        // Create a test file
        fs::write(temp_dir.path().join("test.txt"), "content").unwrap();

        let watcher = FileIndexWatcher::new(temp_dir.path().to_path_buf())
            .await
            .unwrap();

        let index = watcher.get_index().unwrap();
        assert!(!index.files.is_empty());
    }

    #[test]
    fn test_is_source_file() {
        assert!(is_source_file(Path::new("test.rs")));
        assert!(is_source_file(Path::new("test.ts")));
        assert!(is_source_file(Path::new("test.tsx")));
        assert!(is_source_file(Path::new("test.js")));
        assert!(is_source_file(Path::new("test.go")));
        assert!(is_source_file(Path::new("test.java")));
        assert!(!is_source_file(Path::new("test.txt")));
        assert!(!is_source_file(Path::new("test.md")));
    }

    #[test]
    fn test_file_change_set_methods() {
        let changes = vec![
            FileChange {
                path: PathBuf::from("created.rs"),
                kind: FileChangeKind::Created,
                is_dir: false,
            },
            FileChange {
                path: PathBuf::from("modified.ts"),
                kind: FileChangeKind::Modified,
                is_dir: false,
            },
            FileChange {
                path: PathBuf::from("removed.go"),
                kind: FileChangeKind::Removed,
                is_dir: false,
            },
            FileChange {
                path: PathBuf::from("dir"),
                kind: FileChangeKind::Created,
                is_dir: true,
            },
        ];

        let change_set = FileChangeSet {
            changes,
            timestamp: 0,
        };

        let to_reindex: Vec<_> = change_set.files_to_reindex().collect();
        assert_eq!(to_reindex.len(), 2);

        let to_remove: Vec<_> = change_set.files_to_remove().collect();
        assert_eq!(to_remove.len(), 1);

        assert!(change_set.has_source_changes());
    }
}
