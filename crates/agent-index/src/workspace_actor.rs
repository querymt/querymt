//! WorkspaceIndexActor — kameo actor replacing the old `WorkspaceIndex` struct.
//!
//! Owns `FunctionIndex` directly (no `RwLock`). Sequential access is enforced
//! by the actor mailbox.  A background tokio task forwards `FileChangeSet`
//! events from `FileIndexWatcher` as `ApplyChanges` messages to self.

use super::code_cache::{CodeCacheClient, CodeIndexCacheOptions};
use super::error::IndexError;
use super::file_index::{FileChangeSet, FileIndex, FileIndexConfig, FileIndexWatcher};
use super::function_index::{
    FunctionIndex, FunctionIndexConfig, IndexedFunctionEntry, SimilarFunctionMatch,
    collect_source_files, get_language_category,
};
use kameo::Actor;
use kameo::actor::{ActorRef, Spawn};
use kameo::message::{Context, Message};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::instrument;

// ---------------------------------------------------------------------------
// Error type (kept from old workspace.rs)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Statistics type (kept from old workspace.rs)
// ---------------------------------------------------------------------------

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
    /// Per-file extractions that ran since the index started (indexing and tool lookups)
    pub parsed_files: u64,
    /// Per-file results answered from the cache without running since the index started
    pub reused_files: u64,
}

// ---------------------------------------------------------------------------
// WorkspaceHandle — lightweight clone passed to consumers
// ---------------------------------------------------------------------------

/// Lightweight handle returned by `WorkspaceIndexManagerActor::GetOrCreate`.
///
/// Bundles the actor ref (for function index operations) with the file watcher
/// (for lock-free `FileIndex` reads without going through the mailbox).
#[derive(Clone)]
pub struct WorkspaceHandle {
    pub actor: ActorRef<WorkspaceIndexActor>,
    pub file_watcher: Arc<FileIndexWatcher>,
    /// Persistent per-file symbol and function cache of this workspace.
    pub code_cache: CodeCacheClient,
}

impl WorkspaceHandle {
    /// Get the current file index snapshot (lock-free via `ArcSwap`).
    pub fn file_index(&self) -> Option<FileIndex> {
        self.file_watcher.get_index()
    }
}

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// Find functions similar to a given entry.
pub struct FindSimilar {
    pub entry: IndexedFunctionEntry,
}

/// Find functions similar to code in a given file.
pub struct FindSimilarToCode {
    pub file_path: PathBuf,
    pub source: String,
}

/// Update the function index for a file (creates or replaces).
pub struct UpdateFile {
    pub file_path: PathBuf,
    pub source: String,
}

/// Remove a file from the function index.
pub struct RemoveFile {
    pub file_path: PathBuf,
}

/// Apply a batch of file-watcher changes to the function index (internal).
pub(super) struct ApplyChanges {
    pub change_set: FileChangeSet,
}

/// Get the current file index snapshot.
pub struct GetFileIndex;

/// Get workspace statistics.
pub struct GetStats;

/// Force a full rebuild of both indexes.
pub struct Rebuild;

// ---------------------------------------------------------------------------
// Actor definition
// ---------------------------------------------------------------------------

/// Arguments used to construct a `WorkspaceIndexActor`.
pub struct WorkspaceIndexActorArgs {
    pub root: PathBuf,
    pub file_watcher: Arc<FileIndexWatcher>,
    pub function_index: FunctionIndex,
    pub function_config: FunctionIndexConfig,
    pub code_cache: CodeCacheClient,
}

/// Per-workspace actor.  Owns `FileIndexWatcher` (shared) and `FunctionIndex` (owned).
pub struct WorkspaceIndexActor {
    root: PathBuf,
    file_watcher: Arc<FileIndexWatcher>,
    function_index: FunctionIndex,
    function_config: FunctionIndexConfig,
    code_cache: CodeCacheClient,
}

impl Actor for WorkspaceIndexActor {
    type Args = WorkspaceIndexActorArgs;
    type Error = kameo::error::Infallible;

    /// Called once when the actor starts.
    ///
    /// Subscribes to `FileIndexWatcher::subscribe_changes()` and forwards each
    /// `FileChangeSet` to self as an `ApplyChanges` message.  This replaces the
    /// `tokio::spawn` background task in the old `WorkspaceIndex`.
    async fn on_start(args: Self::Args, actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        let mut changes_rx = args.file_watcher.subscribe_changes();
        let actor_ref_clone = actor_ref.clone();
        tokio::spawn(async move {
            while let Ok(change_set) = changes_rx.recv().await {
                if change_set.has_source_changes()
                    && let Err(e) = actor_ref_clone.tell(ApplyChanges { change_set }).await
                {
                    log::debug!("WorkspaceIndexActor: failed to send ApplyChanges: {:?}", e);
                }
            }
        });

        Ok(Self {
            root: args.root,
            file_watcher: args.file_watcher,
            function_index: args.function_index,
            function_config: args.function_config,
            code_cache: args.code_cache,
        })
    }
}

/// Build the function index from the code cache, parsing only files whose text changed.
///
/// Falls back to a direct build when the cache cannot answer, so a cache failure never
/// prevents the workspace index from starting.
async fn build_function_index(
    root: &Path,
    config: &FunctionIndexConfig,
    code_cache: &CodeCacheClient,
) -> Result<FunctionIndex, IndexError> {
    let walk_root = root.to_path_buf();
    let paths = tokio::task::spawn_blocking(move || collect_source_files(&walk_root)).await?;
    match code_cache.index_all(&paths).await {
        Ok(entries) => Ok(FunctionIndex::from_entries(
            root,
            config.clone(),
            entries.into_iter().collect(),
        )),
        Err(error) => {
            log::warn!(
                "WorkspaceIndexActor: code cache failed for {root:?} ({error}); building directly"
            );
            FunctionIndex::build(root, config.clone()).await
        }
    }
}

impl WorkspaceIndexActor {
    /// Create a new `WorkspaceIndexActor` and spawn it, returning a `WorkspaceHandle`.
    ///
    /// # Errors
    ///
    /// Fails if the file watcher cannot start or the cache thread cannot be spawned.
    pub async fn create(
        root: PathBuf,
        file_config: FileIndexConfig,
        function_config: FunctionIndexConfig,
        cache: CodeIndexCacheOptions,
    ) -> Result<WorkspaceHandle, IndexError> {
        let file_watcher =
            Arc::new(FileIndexWatcher::with_config(root.clone(), file_config).await?);

        let code_cache = CodeCacheClient::spawn(&root, cache, function_config.clone())
            .map_err(IndexError::CacheThread)?;
        let function_index = build_function_index(&root, &function_config, &code_cache).await?;

        let file_watcher_clone = file_watcher.clone();
        let actor = WorkspaceIndexActor::spawn(WorkspaceIndexActorArgs {
            root,
            file_watcher: file_watcher_clone.clone(),
            function_index,
            function_config,
            code_cache: code_cache.clone(),
        });

        Ok(WorkspaceHandle {
            actor,
            file_watcher: file_watcher_clone,
            code_cache,
        })
    }

    /// Send one change set to the cache (one Ajika operation) and mirror it into the
    /// in-memory function index. Falls back to direct indexing if the cache fails.
    async fn apply_to_index(&mut self, changed: Vec<(PathBuf, Arc<str>)>, removed: Vec<PathBuf>) {
        // Files without a function-index language never carry entries.
        let (changed, unsupported): (Vec<_>, Vec<_>) =
            changed.into_iter().partition(|(path, _)| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .and_then(get_language_category)
                    .is_some()
            });
        for (path, _) in &unsupported {
            self.function_index.remove_file(path);
        }
        if changed.is_empty() && removed.is_empty() {
            return;
        }
        match self.code_cache.apply(changed.clone(), &removed).await {
            Ok(entries) => {
                for path in &removed {
                    self.function_index.remove_file(path);
                }
                for (path, file_entries) in entries {
                    self.function_index.set_file_entries(&path, file_entries);
                }
            }
            Err(error) => {
                log::warn!(
                    "WorkspaceIndexActor: code cache failed for {:?} ({error}); indexing directly",
                    self.root
                );
                for path in &removed {
                    self.function_index.remove_file(path);
                }
                for (path, source) in &changed {
                    self.function_index.update_file(path, source);
                }
            }
        }
    }

    /// Root directory of this workspace.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

// ---------------------------------------------------------------------------
// Message handlers
// ---------------------------------------------------------------------------

impl Message<FindSimilar> for WorkspaceIndexActor {
    type Reply = Vec<SimilarFunctionMatch>;

    #[instrument(
        name = "workspace_actor.find_similar",
        skip(self, msg, _ctx),
        fields(
            function_name = %msg.entry.name,
            matches = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: FindSimilar,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let results = self.function_index.find_similar(&msg.entry);
        tracing::Span::current().record("matches", results.len());
        results
    }
}

impl Message<FindSimilarToCode> for WorkspaceIndexActor {
    type Reply = Vec<(IndexedFunctionEntry, Vec<SimilarFunctionMatch>)>;

    #[instrument(
        name = "workspace_actor.find_similar_to_code",
        skip(self, msg, _ctx),
        fields(
            file = %msg.file_path.display(),
            results = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: FindSimilarToCode,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let results = self
            .function_index
            .find_similar_to_code(&msg.file_path, &msg.source);
        tracing::Span::current().record("results", results.len());
        results
    }
}

impl Message<UpdateFile> for WorkspaceIndexActor {
    type Reply = ();

    #[instrument(
        name = "workspace_actor.update_file",
        skip(self, msg, _ctx),
        fields(file = %msg.file_path.display())
    )]
    async fn handle(
        &mut self,
        msg: UpdateFile,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let source: Arc<str> = Arc::from(msg.source);
        self.apply_to_index(vec![(msg.file_path, source)], Vec::new())
            .await;
    }
}

impl Message<RemoveFile> for WorkspaceIndexActor {
    type Reply = ();

    #[instrument(
        name = "workspace_actor.remove_file",
        skip(self, msg, _ctx),
        fields(file = %msg.file_path.display())
    )]
    async fn handle(
        &mut self,
        msg: RemoveFile,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.apply_to_index(Vec::new(), vec![msg.file_path]).await;
    }
}

impl Message<ApplyChanges> for WorkspaceIndexActor {
    type Reply = ();

    #[instrument(
        name = "workspace_actor.apply_changes",
        skip(self, msg, _ctx),
        fields(
            files_removed = tracing::field::Empty,
            files_reindexed = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: ApplyChanges,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let change_set = &msg.change_set;

        let remove_count = change_set.files_to_remove().count();
        let reindex_count = change_set.files_to_reindex().count();

        tracing::Span::current().record("files_removed", remove_count);
        tracing::Span::current().record("files_reindexed", reindex_count);

        if remove_count > 0 || reindex_count > 0 {
            log::debug!(
                "WorkspaceIndexActor: Processing {} removals and {} updates",
                remove_count,
                reindex_count
            );
        }

        let removed: Vec<PathBuf> = change_set.files_to_remove().cloned().collect();
        // Created/modified files whose text cannot be read are skipped, as before.
        let changed: Vec<(PathBuf, Arc<str>)> = change_set
            .files_to_reindex()
            .filter_map(|path| {
                let content = std::fs::read_to_string(path).ok()?;
                Some((path.clone(), Arc::from(content)))
            })
            .collect();

        // One cache operation per change set.
        self.apply_to_index(changed, removed).await;
    }
}

impl Message<GetFileIndex> for WorkspaceIndexActor {
    type Reply = Option<FileIndex>;

    async fn handle(
        &mut self,
        _msg: GetFileIndex,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.file_watcher.get_index()
    }
}

impl Message<GetStats> for WorkspaceIndexActor {
    type Reply = Box<WorkspaceIndexStats>;

    async fn handle(
        &mut self,
        _msg: GetStats,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let file_index = self.file_watcher.get_index();
        let file_count = file_index.as_ref().map(|i| i.files.len()).unwrap_or(0);
        let file_index_age_secs = file_index.as_ref().map(|i| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().saturating_sub(i.generated_at))
                .unwrap_or(0)
        });

        let (parsed_files, reused_files) = match self.code_cache.stats().await {
            Ok(stats) => (stats.parsed_files, stats.reused_files),
            Err(error) => {
                log::debug!("WorkspaceIndexActor: code cache stats unavailable: {error}");
                (0, 0)
            }
        };

        Box::new(WorkspaceIndexStats {
            file_count,
            function_count: self.function_index.function_count(),
            indexed_file_count: self.function_index.file_count(),
            file_index_age_secs,
            parsed_files,
            reused_files,
        })
    }
}

impl Message<Rebuild> for WorkspaceIndexActor {
    type Reply = Result<(), IndexError>;

    #[instrument(name = "workspace_actor.rebuild", skip(self, _msg, _ctx))]
    async fn handle(
        &mut self,
        _msg: Rebuild,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Rebuild file index
        self.file_watcher.refresh().await?;

        // Rebuild function index through the cache: unchanged files are reused.
        self.function_index =
            build_function_index(&self.root, &self.function_config, &self.code_cache).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_index::{FileChange, FileChangeKind};
    use std::collections::BTreeSet;

    fn function_source(name: &str) -> String {
        format!(
            "pub fn {name}(values: &[i32]) -> i32 {{\n    let mut total = 0;\n    for value in values {{\n        total += value;\n    }}\n    total\n}}\n"
        )
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        base: PathBuf,
    }

    impl Fixture {
        fn new(files: &[(&str, String)]) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("ws");
            std::fs::create_dir_all(&root).expect("create root");
            for (name, content) in files {
                std::fs::write(root.join(name), content).expect("write file");
            }
            Self {
                base: dir.path().join("cache"),
                root,
                _dir: dir,
            }
        }

        async fn open(&self) -> WorkspaceHandle {
            WorkspaceIndexActor::create(
                self.root.clone(),
                FileIndexConfig::default(),
                FunctionIndexConfig::default(),
                CodeIndexCacheOptions::in_directory(&self.base),
            )
            .await
            .expect("create workspace index")
        }
    }

    /// Release the cache directory so another index can own it.
    async fn close(handle: &WorkspaceHandle) {
        handle.code_cache.shutdown().await;
        let _ = handle.actor.stop_gracefully().await;
    }

    async fn stats(handle: &WorkspaceHandle) -> WorkspaceIndexStats {
        *handle.actor.ask(GetStats).await.expect("stats")
    }

    /// Files and names of the indexed functions that duplicate a probe function.
    async fn duplicates(handle: &WorkspaceHandle, root: &Path) -> BTreeSet<(PathBuf, String)> {
        let results = handle
            .actor
            .ask(FindSimilarToCode {
                file_path: root.join("probe.rs"),
                source: function_source("probe"),
            })
            .await
            .expect("find similar");
        results
            .into_iter()
            .flat_map(|(_, matches)| matches)
            .map(|m| (m.function.file_path, m.function.name))
            .collect()
    }

    async fn apply(handle: &WorkspaceHandle, changes: Vec<(PathBuf, FileChangeKind)>) {
        let change_set = FileChangeSet {
            changes: changes
                .into_iter()
                .map(|(path, kind)| FileChange {
                    path,
                    kind,
                    is_dir: false,
                })
                .collect(),
            timestamp: 0,
        };
        handle
            .actor
            .ask(ApplyChanges { change_set })
            .await
            .expect("apply changes");
    }

    fn three_files() -> [(&'static str, String); 3] {
        [
            ("a.rs", function_source("alpha")),
            ("b.rs", function_source("beta")),
            ("c.rs", function_source("gamma")),
        ]
    }

    #[tokio::test]
    async fn restart_with_unchanged_files_parses_nothing() {
        let fx = Fixture::new(&three_files());

        let first = fx.open().await;
        assert_eq!(stats(&first).await.parsed_files, 3);
        let before = duplicates(&first, &fx.root).await;
        assert_eq!(before.len(), 3, "all three files duplicate the probe");
        close(&first).await;

        let second = fx.open().await;
        let after_start = stats(&second).await;
        assert_eq!(after_start.parsed_files, 0);
        assert_eq!(after_start.reused_files, 3);
        assert_eq!(after_start.indexed_file_count, 3);
        assert_eq!(duplicates(&second, &fx.root).await, before);
    }

    #[tokio::test]
    async fn file_edited_while_the_index_was_closed() {
        let fx = Fixture::new(&three_files());
        let first = fx.open().await;
        close(&first).await;

        std::fs::write(fx.root.join("b.rs"), function_source("beta_renamed")).expect("edit");

        let second = fx.open().await;
        let started = stats(&second).await;
        assert_eq!(started.parsed_files, 1);
        assert_eq!(started.reused_files, 2);

        let found = duplicates(&second, &fx.root).await;
        let b = fx.root.join("b.rs");
        assert!(found.contains(&(b.clone(), "beta_renamed".to_owned())));
        assert!(!found.contains(&(b, "beta".to_owned())));
    }

    #[tokio::test]
    async fn watcher_change_re_parses_only_the_changed_file() {
        let fx = Fixture::new(&three_files());
        let handle = fx.open().await;
        let before = stats(&handle).await;
        assert_eq!(before.parsed_files, 3);

        std::fs::write(fx.root.join("a.rs"), function_source("alpha_modified")).expect("modify");
        std::fs::write(fx.root.join("d.rs"), function_source("delta")).expect("create");
        apply(
            &handle,
            vec![
                (fx.root.join("a.rs"), FileChangeKind::Modified),
                (fx.root.join("d.rs"), FileChangeKind::Created),
            ],
        )
        .await;

        let after = stats(&handle).await;
        assert_eq!(after.parsed_files, before.parsed_files + 2);
        let found = duplicates(&handle, &fx.root).await;
        assert!(found.contains(&(fx.root.join("a.rs"), "alpha_modified".to_owned())));
        assert!(found.contains(&(fx.root.join("d.rs"), "delta".to_owned())));
        assert!(found.contains(&(fx.root.join("b.rs"), "beta".to_owned())));
    }

    #[tokio::test]
    async fn deleted_file_leaves_duplicate_detection() {
        let fx = Fixture::new(&three_files());
        let handle = fx.open().await;
        let a = fx.root.join("a.rs");
        assert!(
            duplicates(&handle, &fx.root)
                .await
                .contains(&(a.clone(), "alpha".to_owned())),
            "the function in file A is matched before removal"
        );

        std::fs::remove_file(&a).expect("remove file");
        apply(&handle, vec![(a.clone(), FileChangeKind::Removed)]).await;

        let found = duplicates(&handle, &fx.root).await;
        assert!(found.iter().all(|(path, _)| path != &a));
        assert_eq!(found.len(), 2, "the other files still match");
        assert_eq!(stats(&handle).await.indexed_file_count, 2);
    }
}
