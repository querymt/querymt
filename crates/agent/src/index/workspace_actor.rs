//! WorkspaceIndexActor — kameo actor replacing the old `WorkspaceIndex` struct.
//!
//! Owns `FunctionIndex` directly (no `RwLock`). Sequential access is enforced
//! by the actor mailbox.  A background tokio task forwards `FileChangeSet`
//! events from `FileIndexWatcher` as `ApplyChanges` messages to self.

use super::file_index::{
    FileChangeSet, FileIndex, FileIndexConfig, FileIndexError, FileIndexWatcher,
};
use super::function_index::{
    FunctionIndex, FunctionIndexConfig, IndexedFunctionEntry, SimilarFunctionMatch,
};
use kameo::Actor;
use kameo::actor::{ActorRef, Spawn};
use kameo::message::{Context, Message};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type (kept from old workspace.rs)
// ---------------------------------------------------------------------------

/// Error types for workspace index operations
#[derive(Debug, Error)]
pub enum WorkspaceIndexError {
    #[error("File index error: {0}")]
    FileIndex(#[from] FileIndexError),
    #[error("Function index error: {0}")]
    FunctionIndex(String),
}

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
}

/// Per-workspace actor.  Owns `FileIndexWatcher` (shared) and `FunctionIndex` (owned).
pub struct WorkspaceIndexActor {
    root: PathBuf,
    file_watcher: Arc<FileIndexWatcher>,
    function_index: FunctionIndex,
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
        })
    }
}

impl WorkspaceIndexActor {
    /// Create a new `WorkspaceIndexActor` and spawn it, returning a `WorkspaceHandle`.
    pub async fn create(
        root: PathBuf,
        file_config: FileIndexConfig,
        function_config: FunctionIndexConfig,
    ) -> Result<WorkspaceHandle, WorkspaceIndexError> {
        let file_watcher =
            Arc::new(FileIndexWatcher::with_config(root.clone(), file_config).await?);

        let function_index = FunctionIndex::build(&root, function_config)
            .await
            .map_err(WorkspaceIndexError::FunctionIndex)?;

        let file_watcher_clone = file_watcher.clone();
        let actor = WorkspaceIndexActor::spawn(WorkspaceIndexActorArgs {
            root,
            file_watcher: file_watcher_clone.clone(),
            function_index,
        });

        Ok(WorkspaceHandle {
            actor,
            file_watcher: file_watcher_clone,
        })
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

    async fn handle(
        &mut self,
        msg: FindSimilar,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.function_index.find_similar(&msg.entry)
    }
}

impl Message<FindSimilarToCode> for WorkspaceIndexActor {
    type Reply = Vec<(IndexedFunctionEntry, Vec<SimilarFunctionMatch>)>;

    async fn handle(
        &mut self,
        msg: FindSimilarToCode,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.function_index
            .find_similar_to_code(&msg.file_path, &msg.source)
    }
}

impl Message<UpdateFile> for WorkspaceIndexActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: UpdateFile,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.function_index.update_file(&msg.file_path, &msg.source);
    }
}

impl Message<RemoveFile> for WorkspaceIndexActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: RemoveFile,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.function_index.remove_file(&msg.file_path);
    }
}

impl Message<ApplyChanges> for WorkspaceIndexActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: ApplyChanges,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let change_set = &msg.change_set;

        let remove_count = change_set.files_to_remove().count();
        let reindex_count = change_set.files_to_reindex().count();

        if remove_count > 0 || reindex_count > 0 {
            log::debug!(
                "WorkspaceIndexActor: Processing {} removals and {} updates",
                remove_count,
                reindex_count
            );
        }

        // Remove deleted files
        for path in change_set.files_to_remove() {
            self.function_index.remove_file(path);
            log::debug!("WorkspaceIndexActor: Removed {:?}", path);
        }

        // Reindex created/modified files (sequential — actor mailbox enforces this)
        for path in change_set.files_to_reindex() {
            if let Ok(content) = std::fs::read_to_string(path) {
                self.function_index.update_file(path, &content);
                log::debug!("WorkspaceIndexActor: Updated {:?}", path);
            }
        }
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

        Box::new(WorkspaceIndexStats {
            file_count,
            function_count: self.function_index.function_count(),
            indexed_file_count: self.function_index.file_count(),
            file_index_age_secs,
        })
    }
}

impl Message<Rebuild> for WorkspaceIndexActor {
    type Reply = Result<(), String>;

    async fn handle(
        &mut self,
        _msg: Rebuild,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Rebuild file index
        self.file_watcher
            .refresh()
            .await
            .map_err(|e| e.to_string())?;

        // Rebuild function index
        let new_index = FunctionIndex::build(&self.root, FunctionIndexConfig::default()).await?;

        self.function_index = new_index;
        Ok(())
    }
}
