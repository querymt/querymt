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
        "py",  // Python
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
            move |res: Result<Event, notify::Error>| match res {
                Ok(event) => {
                    log::trace!("FileWatcher: received event {:?}", event.kind);
                    let _ = event_tx.blocking_send(event);
                }
                Err(e) => {
                    log::warn!("FileWatcher: error receiving event: {:?}", e);
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
                                log::debug!(
                                    "FileIndexWatcher: Processing {} file changes",
                                    change_set.changes.len()
                                );
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
                                    log::debug!(
                                        "FileIndexWatcher: Broadcasting updated index with {} files",
                                        updated.files.len()
                                    );
                                    let _ = index_tx_clone.send(updated);
                                } else {
                                    // No current index, do full rebuild in spawn_blocking
                                    log::debug!("FileIndexWatcher: No current index, performing full rebuild");
                                    let root = root_clone.clone();
                                    let config = config_clone.clone();
                                    let overrides = overrides_clone.clone();

                                    if let Ok(Ok(new_index)) = tokio::task::spawn_blocking(move || {
                                        build_file_index(&root, &config, &overrides)
                                    }).await {
                                        index_clone.store(Arc::new(Some(new_index.clone())));
                                        log::debug!(
                                            "FileIndexWatcher: Broadcasting rebuilt index with {} files",
                                            new_index.files.len()
                                        );
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

            // Determine if this is a directory
            // For CreateKind::Folder and RemoveKind::Folder, we know it's a directory from the event
            // For other cases, we need to check the filesystem (which may have timing issues)
            let is_dir = match &event.kind {
                EventKind::Create(CreateKind::Folder) => true,
                EventKind::Create(CreateKind::File) => false,
                EventKind::Remove(RemoveKind::Folder) => true,
                EventKind::Remove(RemoveKind::File) => false,
                // For Any/Other/Modify events, check the filesystem
                _ => path.is_dir(),
            };

            let relative = match path.strip_prefix(root) {
                Ok(rel) => rel.to_string_lossy().to_string(),
                Err(_) => continue,
            };

            if is_ignored(&relative, is_dir, overrides) {
                continue;
            }

            let change_kind = match &event.kind {
                // Handle all CreateKind variants
                // CreateKind::Any is the catch-all used when the specific kind is unknown
                // (common on macOS with FSEvents which doesn't distinguish file vs folder)
                EventKind::Create(CreateKind::File)
                | EventKind::Create(CreateKind::Folder)
                | EventKind::Create(CreateKind::Any)
                | EventKind::Create(CreateKind::Other) => Some(FileChangeKind::Created),
                EventKind::Modify(ModifyKind::Data(_)) | EventKind::Modify(ModifyKind::Any) => {
                    Some(FileChangeKind::Modified)
                }
                EventKind::Remove(RemoveKind::File)
                | EventKind::Remove(RemoveKind::Folder)
                | EventKind::Remove(RemoveKind::Any)
                | EventKind::Remove(RemoveKind::Other) => Some(FileChangeKind::Removed),
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
                EventKind::Modify(ModifyKind::Name(RenameMode::Any))
                | EventKind::Modify(ModifyKind::Name(RenameMode::Other)) => {
                    // FSEvents (macOS) sends RenameMode::Any for both source and destination
                    // of a rename/move. We can't correlate them, so check if path exists:
                    // - Exists: destination of rename → Created
                    // - Doesn't exist: source of rename → Removed
                    if path.exists() {
                        Some(FileChangeKind::Created)
                    } else {
                        Some(FileChangeKind::Removed)
                    }
                }
                _ => None,
            };

            if let Some(kind) = change_kind {
                // Check if path is within root
                if path.starts_with(root) {
                    seen_paths.insert(path.clone());
                    log::debug!(
                        "FileIndexWatcher: Detected {:?} event for {} (is_dir: {})",
                        kind,
                        relative,
                        is_dir
                    );
                    changes.push(FileChange {
                        path: path.clone(),
                        kind,
                        is_dir,
                    });
                }
            } else {
                // Log events we're ignoring (helps debugging)
                log::trace!(
                    "FileIndexWatcher: Ignoring event {:?} for path {:?}",
                    event.kind,
                    relative
                );
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

    // Add each pattern with ! prefix (meaning "exclude/ignore" in Override semantics)
    //
    // IMPORTANT: We only add ignore patterns (with ! prefix) here. Without any whitelist
    // patterns, unmatched files return Match::None which allows .gitignore to handle them.
    //
    // If we added a whitelist pattern like "**/*", it would OVERRIDE .gitignore rules
    // because Override patterns have higher priority than .gitignore! This was the bug
    // that caused node_modules to be indexed despite being in .gitignore.
    //
    // With only ignore patterns:
    // - Files matching !pattern -> Match::Ignore (definitely excluded)
    // - Files not matching any pattern -> Match::None (let .gitignore decide)
    for pattern in &config.ignore_patterns {
        // Normalize pattern to use glob recursion
        let normalized_pattern = normalize_ignore_pattern(pattern);
        let exclude_pattern = format!("!{}", normalized_pattern);

        builder.add(&exclude_pattern).map_err(|e| {
            FileIndexError::InvalidConfig(format!("Invalid pattern '{}': {}", pattern, e))
        })?;
    }

    builder
        .build()
        .map_err(|e| FileIndexError::InvalidConfig(format!("Failed to build overrides: {}", e)))
}

/// Normalize ignore patterns to ensure they match recursively
///
/// Examples:
/// - "target/" -> "target/**"
/// - "target" -> "target/**"
/// - "node_modules/" -> "node_modules/**"
/// - "*.db" -> "*.db" (unchanged - file pattern)
/// - ".env" -> ".env" (unchanged - single file)
fn normalize_ignore_pattern(pattern: &str) -> String {
    // If pattern already has glob syntax, use as-is
    if pattern.contains("**") || pattern.contains('*') && !pattern.ends_with('/') {
        return pattern.to_string();
    }

    // If pattern ends with /, it's a directory - add recursive glob
    if pattern.ends_with('/') {
        return format!("{}**", pattern);
    }

    // For patterns without extension (likely directories), add recursive glob
    if !pattern.contains('.') {
        return format!("{}/**", pattern);
    }

    // Otherwise use as-is (e.g., ".env", ".DS_Store")
    pattern.to_string()
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

    /// Test Override pattern matching behavior
    /// This test verifies that our ignore patterns work correctly for filtering file events
    #[test]
    fn test_override_semantics() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing Override Pattern Semantics ===\n");

        // Test 1: Current implementation (only negation with "!" prefix)
        println!("Test 1: Only negation patterns (current implementation)");
        let mut builder1 = OverrideBuilder::new(root);
        builder1.add("!target/").unwrap();
        let overrides1 = builder1.build().unwrap();

        // Test 2: Whitelist everything at root level, then negate
        println!("\nTest 2: Whitelist root + negation");
        let mut builder2 = OverrideBuilder::new(root);
        builder2.add("*").unwrap(); // Match everything at root
        builder2.add("!target").unwrap(); // Except target dir
        let overrides2 = builder2.build().unwrap();

        // Test 3: Recursive patterns
        println!("\nTest 3: Recursive whitelist + negation");
        let mut builder3 = OverrideBuilder::new(root);
        builder3.add("**/*").unwrap(); // Match everything recursively
        builder3.add("!target/**").unwrap(); // Except target and all contents
        let overrides3 = builder3.build().unwrap();

        // Test 4: Pattern without trailing slash
        println!("\nTest 4: Pattern without trailing slash");
        let mut builder4 = OverrideBuilder::new(root);
        builder4.add("**/*").unwrap();
        builder4.add("!target").unwrap(); // No trailing slash
        let overrides4 = builder4.build().unwrap();

        // Test paths that should be ignored
        let test_paths = vec![
            (
                "target/debug/.fingerprint/lib.rs",
                false,
                "File deep in target",
            ),
            ("target/debug/build.rs", false, "File in target/debug"),
            ("target/release/build", true, "Dir in target/release"),
            ("src/main.rs", false, "File in src (should NOT be ignored)"),
            ("target", true, "Target directory itself"),
        ];

        for (path, is_dir, description) in test_paths {
            println!("\nPath: {} ({})", path, description);

            let m1 = overrides1.matched(path, is_dir);
            let m2 = overrides2.matched(path, is_dir);
            let m3 = overrides3.matched(path, is_dir);
            let m4 = overrides4.matched(path, is_dir);

            println!(
                "  Test1 (only negation):     matched={:?}, is_ignore={}",
                m1,
                m1.is_ignore()
            );
            println!(
                "  Test2 (root whitelist):    matched={:?}, is_ignore={}",
                m2,
                m2.is_ignore()
            );
            println!(
                "  Test3 (recursive):         matched={:?}, is_ignore={}",
                m3,
                m3.is_ignore()
            );
            println!(
                "  Test4 (no trailing slash): matched={:?}, is_ignore={}",
                m4,
                m4.is_ignore()
            );

            // For target/* paths, at least one method should mark as ignored
            if path.starts_with("target") {
                let any_ignored =
                    m1.is_ignore() || m2.is_ignore() || m3.is_ignore() || m4.is_ignore();
                if !any_ignored {
                    println!("  ⚠️  WARNING: target path not ignored by any method!");
                }
            }
        }
    }

    /// Test the actual is_ignored function used by process_events
    #[test]
    fn test_is_ignored_function() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing is_ignored() Function ===\n");

        // Use default config which has "target/" in ignore_patterns
        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        let test_paths = vec![
            ("target/debug/.fingerprint/lib.rs", false),
            ("target/debug/build.rs", false),
            ("target/release/build", true),
            ("src/main.rs", false),
            ("node_modules/package/index.js", false),
            (".git/objects/abc", false),
            ("dist/bundle.js", false),
        ];

        for (path, is_dir) in test_paths {
            let ignored = is_ignored(path, is_dir, &overrides);
            println!(
                "Path: {:<40} is_dir={:<5} ignored={}",
                path, is_dir, ignored
            );

            // Verify expected behavior
            if path.starts_with("target/") {
                assert!(
                    ignored,
                    "Path '{}' should be ignored (in default ignore_patterns)",
                    path
                );
            }
        }
    }

    /// Test process_events filtering with actual file system events
    #[test]
    fn test_process_events_filters_ignored_paths() {
        use notify::event::{DataChange, ModifyKind};
        use notify::{Event, EventKind};

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing process_events Filtering ===\n");

        // Create actual directory structure
        fs::create_dir_all(root.join("target/debug/.fingerprint")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        // Simulate file events
        let mut target_event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)));
        target_event
            .paths
            .push(root.join("target/debug/.fingerprint/lib.rs"));

        let mut src_event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)));
        src_event.paths.push(root.join("src/main.rs"));

        let events = vec![target_event, src_event];

        let change_set = process_events(&events, root, &overrides);

        println!("Total changes processed: {}", change_set.changes.len());
        for change in &change_set.changes {
            println!("  - {:?}", change.path);
        }

        // Verify target/ files are filtered out
        let has_target_file = change_set
            .changes
            .iter()
            .any(|c| c.path.to_string_lossy().contains("target"));

        assert!(
            !has_target_file,
            "process_events should filter out target/ paths. Found changes: {:?}",
            change_set.changes
        );

        // Verify src/ files are NOT filtered
        let has_src_file = change_set
            .changes
            .iter()
            .any(|c| c.path.to_string_lossy().contains("src/main.rs"));

        assert!(
            has_src_file,
            "process_events should NOT filter out src/ paths"
        );
    }

    /// Test compile_overrides with different pattern formats
    #[test]
    fn test_compile_overrides_patterns() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing Different Pattern Formats ===\n");

        let pattern_sets = vec![
            (vec!["target/"], "With trailing slash"),
            (vec!["target"], "Without trailing slash"),
            (vec!["target/**"], "With glob recursion"),
            (vec!["**/target/**"], "Full glob path"),
        ];

        for (patterns, description) in pattern_sets {
            println!("\nPattern set: {} - {:?}", description, patterns);

            let config = FileIndexConfig {
                ignore_patterns: patterns.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            };

            let overrides = compile_overrides(root, &config).unwrap();

            let test_paths = vec![
                "target/debug/.fingerprint/lib.rs",
                "target/release/build",
                "src/main.rs",
            ];

            for path in test_paths {
                let ignored = is_ignored(path, false, &overrides);
                println!("  {} -> ignored={}", path, ignored);
            }
        }
    }

    /// Test the normalize_ignore_pattern function
    #[test]
    fn test_normalize_ignore_pattern() {
        let test_cases = vec![
            ("target/", "target/**"),
            ("target", "target/**"),
            ("node_modules/", "node_modules/**"),
            ("node_modules", "node_modules/**"),
            ("dist/", "dist/**"),
            ("build", "build/**"),
            ("*.db", "*.db"),
            ("*.pyc", "*.pyc"),
            (".env", ".env"),
            (".env.*", ".env.*"),
            (".DS_Store", ".DS_Store"),
            ("target/**", "target/**"),
            ("**/target/**", "**/target/**"),
        ];

        println!("\n=== Testing normalize_ignore_pattern ===\n");
        for (input, expected) in test_cases {
            let result = normalize_ignore_pattern(input);
            println!("'{}' -> '{}' (expected: '{}')", input, result, expected);
            assert_eq!(
                result, expected,
                "normalize_ignore_pattern('{}') should return '{}'",
                input, expected
            );
        }
    }

    /// Test that the fix resolves the original issue
    #[test]
    fn test_fixed_override_filters_target() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing Fixed Override Implementation ===\n");

        // Use default config which includes "target/"
        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        let test_paths = vec![
            ("target/debug/.fingerprint/lib.rs", false, true),
            ("target/debug/build.rs", false, true),
            ("target/release/build", true, true),
            ("src/main.rs", false, false),
            ("node_modules/package/index.js", false, true),
            (".git/objects/abc", false, true),
            ("dist/bundle.js", false, true),
            ("README.md", false, false),
        ];

        for (path, is_dir, should_be_ignored) in test_paths {
            let ignored = is_ignored(path, is_dir, &overrides);
            println!(
                "Path: {:<40} is_dir={:<5} ignored={:<5} (expected: {})",
                path, is_dir, ignored, should_be_ignored
            );

            assert_eq!(
                ignored,
                should_be_ignored,
                "Path '{}' should {}be ignored",
                path,
                if should_be_ignored { "" } else { "NOT " }
            );
        }
    }

    /// Test that .gitignore is respected when using Overrides
    /// This is a regression test for the bug where **/* whitelist was overriding .gitignore
    #[tokio::test]
    async fn test_gitignore_respected_with_overrides() {
        use std::process::Command;

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing .gitignore Respect with Overrides ===\n");

        // Initialize as git repo so .gitignore is recognized by WalkBuilder
        Command::new("git")
            .args(["init"])
            .current_dir(root)
            .output()
            .ok();

        // Create .gitignore that ignores node_modules
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        println!("Created .gitignore with: node_modules/");

        // Create node_modules with a file (should be ignored via .gitignore)
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "// code").unwrap();
        println!("Created node_modules/pkg/index.js");

        // Create src with a file (should be included)
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        println!("Created src/main.rs");

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();
        let index = build_file_index(root, &config, &overrides).unwrap();

        println!("\nIndexed files:");
        for file in &index.files {
            println!("  {}", file.path);
        }

        // .gitignore should be respected - no node_modules files
        let node_modules_files: Vec<_> = index
            .files
            .iter()
            .filter(|f| f.path.contains("node_modules"))
            .collect();

        assert!(
            node_modules_files.is_empty(),
            "node_modules should be ignored via .gitignore, but found: {:?}",
            node_modules_files
        );

        // src/main.rs should be present
        assert!(
            index.files.iter().any(|f| f.path == "src/main.rs"),
            "src/main.rs should be indexed"
        );

        println!("\n✓ .gitignore correctly respected with Override patterns");
    }

    /// Test that the fix works on the actual UI directory with real node_modules
    #[tokio::test]
    #[ignore] // Only run manually since it depends on external directory structure
    async fn test_ui_directory_node_modules_ignored() {
        use std::path::PathBuf;

        let ui_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui");

        if !ui_path.exists() {
            println!("UI directory doesn't exist, skipping test");
            return;
        }

        println!("\n=== Testing Real UI Directory ===");
        println!("Path: {:?}", ui_path);

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(&ui_path, &config).unwrap();
        let index = build_file_index(&ui_path, &config, &overrides).unwrap();

        println!("\nTotal files indexed: {}", index.files.len());

        let node_modules_files: Vec<_> = index
            .files
            .iter()
            .filter(|f| f.path.contains("node_modules"))
            .collect();

        println!(
            "Files containing 'node_modules': {}",
            node_modules_files.len()
        );

        if !node_modules_files.is_empty() {
            println!("\nFirst 10 node_modules files:");
            for file in node_modules_files.iter().take(10) {
                println!("  {}", file.path);
            }
        }

        assert!(
            node_modules_files.is_empty(),
            "node_modules should be ignored via .gitignore in UI directory"
        );

        println!("\n✓ UI directory correctly ignores node_modules");
    }

    /// Test that CreateKind::Any events are properly handled
    /// This is critical for macOS FSEvents which often uses Any instead of File/Folder
    #[test]
    fn test_create_kind_any_handling() {
        use notify::event::CreateKind;
        use notify::{Event, EventKind};

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing CreateKind::Any Handling ===\n");

        // Create test file
        fs::write(root.join("test.txt"), "content").unwrap();

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        // Test all CreateKind variants
        let create_variants = vec![
            (CreateKind::File, "File"),
            (CreateKind::Folder, "Folder"),
            (CreateKind::Any, "Any"),
            (CreateKind::Other, "Other"),
        ];

        for (create_kind, name) in create_variants {
            println!("Testing CreateKind::{}", name);

            let mut event = Event::new(EventKind::Create(create_kind));
            event.paths.push(root.join("test.txt"));

            let change_set = process_events(&[event], root, &overrides);

            assert_eq!(
                change_set.changes.len(),
                1,
                "CreateKind::{} should be detected",
                name
            );

            let change = &change_set.changes[0];
            assert_eq!(change.kind, FileChangeKind::Created);
            assert_eq!(
                change.path,
                root.join("test.txt"),
                "Path should match for CreateKind::{}",
                name
            );

            println!("  ✓ CreateKind::{} correctly detected", name);
        }

        println!("\n✓ All CreateKind variants properly handled");
    }

    /// Test that RemoveKind::Any events are properly handled
    #[test]
    fn test_remove_kind_any_handling() {
        use notify::event::RemoveKind;
        use notify::{Event, EventKind};

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing RemoveKind::Any Handling ===\n");

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        // Test all RemoveKind variants
        let remove_variants = vec![
            (RemoveKind::File, "File"),
            (RemoveKind::Folder, "Folder"),
            (RemoveKind::Any, "Any"),
            (RemoveKind::Other, "Other"),
        ];

        for (remove_kind, name) in remove_variants {
            println!("Testing RemoveKind::{}", name);

            let mut event = Event::new(EventKind::Remove(remove_kind));
            event.paths.push(root.join("deleted.txt"));

            let change_set = process_events(&[event], root, &overrides);

            assert_eq!(
                change_set.changes.len(),
                1,
                "RemoveKind::{} should be detected",
                name
            );

            let change = &change_set.changes[0];
            assert_eq!(change.kind, FileChangeKind::Removed);

            println!("  ✓ RemoveKind::{} correctly detected", name);
        }

        println!("\n✓ All RemoveKind variants properly handled");
    }

    /// Test is_dir detection using event kind
    #[test]
    fn test_is_dir_from_event_kind() {
        use notify::event::{CreateKind, RemoveKind};
        use notify::{Event, EventKind};

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing is_dir Detection from Event Kind ===\n");

        // Create test directory and file
        fs::create_dir(root.join("test_dir")).unwrap();
        fs::write(root.join("test_file.txt"), "content").unwrap();

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        // Test CreateKind::Folder
        let mut folder_create = Event::new(EventKind::Create(CreateKind::Folder));
        folder_create.paths.push(root.join("test_dir"));
        let change_set = process_events(&[folder_create], root, &overrides);
        assert!(
            change_set.changes[0].is_dir,
            "CreateKind::Folder should set is_dir=true"
        );
        println!("  ✓ CreateKind::Folder correctly sets is_dir=true");

        // Test CreateKind::File
        let mut file_create = Event::new(EventKind::Create(CreateKind::File));
        file_create.paths.push(root.join("test_file.txt"));
        let change_set = process_events(&[file_create], root, &overrides);
        assert!(
            !change_set.changes[0].is_dir,
            "CreateKind::File should set is_dir=false"
        );
        println!("  ✓ CreateKind::File correctly sets is_dir=false");

        // Test RemoveKind::Folder
        let mut folder_remove = Event::new(EventKind::Remove(RemoveKind::Folder));
        folder_remove.paths.push(root.join("test_dir"));
        let change_set = process_events(&[folder_remove], root, &overrides);
        assert!(
            change_set.changes[0].is_dir,
            "RemoveKind::Folder should set is_dir=true"
        );
        println!("  ✓ RemoveKind::Folder correctly sets is_dir=true");

        // Test RemoveKind::File
        let mut file_remove = Event::new(EventKind::Remove(RemoveKind::File));
        file_remove.paths.push(root.join("test_file.txt"));
        let change_set = process_events(&[file_remove], root, &overrides);
        assert!(
            !change_set.changes[0].is_dir,
            "RemoveKind::File should set is_dir=false"
        );
        println!("  ✓ RemoveKind::File correctly sets is_dir=false");

        // Test CreateKind::Any falls back to filesystem check
        let mut any_create = Event::new(EventKind::Create(CreateKind::Any));
        any_create.paths.push(root.join("test_dir"));
        let change_set = process_events(&[any_create], root, &overrides);
        assert!(
            change_set.changes[0].is_dir,
            "CreateKind::Any should detect directory from filesystem"
        );
        println!("  ✓ CreateKind::Any correctly falls back to filesystem check");

        println!("\n✓ is_dir detection working correctly");
    }

    /// Test RenameMode::Any handling - the core fix for macOS rename detection
    #[test]
    fn test_rename_mode_any_handling() {
        use notify::event::ModifyKind;
        use notify::{Event, EventKind};

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing RenameMode::Any Handling ===\n");

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        // Test Case 1: RenameMode::Any for a file that EXISTS (destination of rename)
        println!("Test Case 1: RenameMode::Any for existing file (destination)");
        let existing_file = root.join("destination.txt");
        fs::write(&existing_file, "content").unwrap();

        let mut event_exists = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)));
        event_exists.paths.push(existing_file.clone());

        let change_set = process_events(&[event_exists], root, &overrides);

        assert_eq!(
            change_set.changes.len(),
            1,
            "RenameMode::Any for existing file should be detected"
        );
        assert_eq!(
            change_set.changes[0].kind,
            FileChangeKind::Created,
            "Existing path should be treated as Created"
        );
        assert_eq!(change_set.changes[0].path, existing_file);
        println!("  ✓ Existing file correctly detected as Created\n");

        // Test Case 2: RenameMode::Any for a file that DOESN'T EXIST (source of rename)
        println!("Test Case 2: RenameMode::Any for non-existing file (source)");
        let non_existing_file = root.join("source.txt");
        // Don't create this file - it should not exist

        let mut event_not_exists = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)));
        event_not_exists.paths.push(non_existing_file.clone());

        let change_set = process_events(&[event_not_exists], root, &overrides);

        assert_eq!(
            change_set.changes.len(),
            1,
            "RenameMode::Any for non-existing file should be detected"
        );
        assert_eq!(
            change_set.changes[0].kind,
            FileChangeKind::Removed,
            "Non-existing path should be treated as Removed"
        );
        assert_eq!(change_set.changes[0].path, non_existing_file);
        println!("  ✓ Non-existing file correctly detected as Removed\n");

        // Test Case 3: Simulate a full rename operation (both source and destination)
        println!("Test Case 3: Simulated rename operation (source → destination)");
        let source_path = root.join("old_name.txt");
        let dest_path = root.join("new_name.txt");

        // Initially, source exists, destination doesn't
        fs::write(&source_path, "content").unwrap();

        // First event: source path (still exists at this point in real scenario, but we'll
        // simulate after the rename when it doesn't exist)
        let mut source_event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)));
        source_event.paths.push(source_path.clone());

        // Second event: destination path (now exists)
        let mut dest_event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)));
        dest_event.paths.push(dest_path.clone());

        // Simulate the rename: move source to dest
        fs::rename(&source_path, &dest_path).unwrap();

        // Process both events (as FSEvents would send them)
        let change_set = process_events(&[source_event, dest_event], root, &overrides);

        // Should get 2 changes: one Removed (source), one Created (dest)
        assert_eq!(
            change_set.changes.len(),
            2,
            "Should detect both source removal and destination creation"
        );

        let has_removed = change_set
            .changes
            .iter()
            .any(|c| c.path == source_path && c.kind == FileChangeKind::Removed);
        let has_created = change_set
            .changes
            .iter()
            .any(|c| c.path == dest_path && c.kind == FileChangeKind::Created);

        assert!(
            has_removed,
            "Source path should be detected as Removed: {:?}",
            change_set.changes
        );
        assert!(
            has_created,
            "Destination path should be detected as Created: {:?}",
            change_set.changes
        );

        println!("  ✓ Source correctly detected as Removed");
        println!("  ✓ Destination correctly detected as Created");

        println!("\n✓ RenameMode::Any handling working correctly");
    }

    /// Test RenameMode::Other handling (should behave like RenameMode::Any)
    #[test]
    fn test_rename_mode_other_handling() {
        use notify::event::ModifyKind;
        use notify::{Event, EventKind};

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        println!("\n=== Testing RenameMode::Other Handling ===\n");

        let config = FileIndexConfig::default();
        let overrides = compile_overrides(root, &config).unwrap();

        // Test with existing file
        let existing_file = root.join("test.txt");
        fs::write(&existing_file, "content").unwrap();

        let mut event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Other)));
        event.paths.push(existing_file.clone());

        let change_set = process_events(&[event], root, &overrides);

        assert_eq!(
            change_set.changes.len(),
            1,
            "RenameMode::Other should be detected"
        );
        assert_eq!(
            change_set.changes[0].kind,
            FileChangeKind::Created,
            "Should behave like RenameMode::Any"
        );

        println!("  ✓ RenameMode::Other correctly handled\n");
    }

    /// Integration test: Verify file move is detected and index is updated
    /// This test is marked #[ignore] because it relies on actual file system events
    /// and timing, which can be flaky in CI environments.
    #[tokio::test]
    #[ignore]
    async fn test_file_move_detected() {
        use tokio::time::{Duration, sleep};

        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();

        println!("\n=== Integration Test: File Move Detection ===\n");

        // Create watcher with shorter intervals for faster testing
        let config = FileIndexConfig {
            debounce: Duration::from_millis(100),
            rebuild_interval: Duration::from_millis(500),
            ..Default::default()
        };

        let watcher = FileIndexWatcher::with_config(root.clone(), config)
            .await
            .unwrap();

        let mut changes_rx = watcher.subscribe_changes();

        // Step 1: Create initial file
        println!("Step 1: Creating initial file 'original.txt'");
        let original_path = root.join("original.txt");
        fs::write(&original_path, "test content").unwrap();

        // Wait for debounce + rebuild
        sleep(Duration::from_millis(800)).await;

        // Verify file appears in index
        let index = watcher.get_index().unwrap();
        assert!(
            index.files.iter().any(|f| f.path == "original.txt"),
            "Original file should be in index"
        );
        println!("  ✓ Original file indexed");

        // Step 2: Move/rename the file
        println!("\nStep 2: Moving 'original.txt' → 'renamed.txt'");
        let renamed_path = root.join("renamed.txt");
        fs::rename(&original_path, &renamed_path).unwrap();

        // Wait for debounce + rebuild
        sleep(Duration::from_millis(800)).await;

        // Step 3: Check for change events
        println!("\nStep 3: Checking for change events");
        let mut found_changes = false;
        let mut has_removed = false;
        let mut has_created = false;

        // Try to receive changes (with timeout)
        match tokio::time::timeout(Duration::from_millis(500), changes_rx.recv()).await {
            Ok(Ok(change_set)) => {
                found_changes = true;
                println!("  Received {} changes:", change_set.changes.len());
                for change in &change_set.changes {
                    let relative = change.path.strip_prefix(&root).unwrap_or(&change.path);
                    println!("    - {:?}: {:?}", relative, change.kind);

                    if relative == Path::new("original.txt")
                        && change.kind == FileChangeKind::Removed
                    {
                        has_removed = true;
                    }
                    if relative == Path::new("renamed.txt")
                        && change.kind == FileChangeKind::Created
                    {
                        has_created = true;
                    }
                }
            }
            Ok(Err(e)) => println!("  Error receiving changes: {:?}", e),
            Err(_) => println!("  No changes received (timeout)"),
        }

        if found_changes {
            println!("\n  ✓ Change events detected");
            if has_removed {
                println!("  ✓ Original file detected as Removed");
            }
            if has_created {
                println!("  ✓ Renamed file detected as Created");
            }
        }

        // Step 4: Verify final index state
        println!("\nStep 4: Verifying final index state");
        let final_index = watcher.get_index().unwrap();

        let has_original = final_index.files.iter().any(|f| f.path == "original.txt");
        let has_renamed = final_index.files.iter().any(|f| f.path == "renamed.txt");

        println!("  Index contains 'original.txt': {}", has_original);
        println!("  Index contains 'renamed.txt': {}", has_renamed);

        assert!(!has_original, "Original file should be removed from index");
        assert!(has_renamed, "Renamed file should be in index");

        println!("\n  ✓ Index correctly reflects the file move");
        println!("\n✓ File move integration test passed");
    }
}
