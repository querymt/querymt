//! WorkspaceIndexManagerActor — kameo actor replacing `WorkspaceIndexManager`.
//!
//! Manages a pool of `WorkspaceIndexActor` instances, one per workspace root.
//! TTL-based eviction is performed on each `GetOrCreate` call.

use super::file_index::FileIndexConfig;
use super::function_index::FunctionIndexConfig;
use super::workspace_actor::{WorkspaceHandle, WorkspaceIndexActor};
use kameo::Actor;
use kameo::actor::{ActorRef, Spawn};
use kameo::message::{Context, Message};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Configuration (moved from old workspace_manager.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct WorkspaceIndexManagerConfig {
    pub max_indexes: usize,
    pub idle_ttl: Duration,
}

impl Default for WorkspaceIndexManagerConfig {
    fn default() -> Self {
        Self {
            max_indexes: 5,
            idle_ttl: Duration::from_secs(15 * 60),
        }
    }
}

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// Get or create a `WorkspaceIndexActor` for the given root directory.
///
/// Spawns a new actor if one does not exist; returns the existing handle otherwise.
/// Also prunes idle workspaces on each access.
pub struct GetOrCreate {
    pub root: PathBuf,
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct ManagedWorkspace {
    handle: WorkspaceHandle,
    last_access: Instant,
}

// ---------------------------------------------------------------------------
// Actor definition
// ---------------------------------------------------------------------------

/// Arguments used to construct a `WorkspaceIndexManagerActor`.
pub struct WorkspaceIndexManagerActorArgs {
    pub config: WorkspaceIndexManagerConfig,
}

/// Actor that manages the pool of `WorkspaceIndexActor` instances.
pub struct WorkspaceIndexManagerActor {
    config: WorkspaceIndexManagerConfig,
    indexes: HashMap<PathBuf, ManagedWorkspace>,
}

impl Actor for WorkspaceIndexManagerActor {
    type Args = WorkspaceIndexManagerActorArgs;
    type Error = kameo::error::Infallible;

    async fn on_start(args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Self {
            config: args.config,
            indexes: HashMap::new(),
        })
    }
}

impl WorkspaceIndexManagerActor {
    /// Spawn a new manager actor with the given config.
    pub fn new(config: WorkspaceIndexManagerConfig) -> ActorRef<Self> {
        WorkspaceIndexManagerActor::spawn(WorkspaceIndexManagerActorArgs { config })
    }
}

// ---------------------------------------------------------------------------
// Message handlers
// ---------------------------------------------------------------------------

impl Message<GetOrCreate> for WorkspaceIndexManagerActor {
    type Reply = Result<WorkspaceHandle, String>;

    async fn handle(
        &mut self,
        msg: GetOrCreate,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let root = msg.root;

        // Return existing handle if present and update last_access.
        if let Some(managed) = self.indexes.get_mut(&root) {
            managed.last_access = Instant::now();
            let handle = managed.handle.clone();
            prune_indexes(&mut self.indexes, &self.config);
            return Ok(handle);
        }

        // Build a new workspace actor.
        log::debug!(
            "WorkspaceIndexManagerActor: initializing workspace {:?}",
            root
        );
        let handle = WorkspaceIndexActor::create(
            root.clone(),
            FileIndexConfig::default(),
            FunctionIndexConfig::default(),
        )
        .await
        .map_err(|e| e.to_string())?;

        self.indexes.insert(
            root,
            ManagedWorkspace {
                handle: handle.clone(),
                last_access: Instant::now(),
            },
        );

        prune_indexes(&mut self.indexes, &self.config);
        Ok(handle)
    }
}

// ---------------------------------------------------------------------------
// Pruning logic (same TTL + max-count logic as old workspace_manager.rs)
// ---------------------------------------------------------------------------

fn prune_indexes(
    indexes: &mut HashMap<PathBuf, ManagedWorkspace>,
    config: &WorkspaceIndexManagerConfig,
) {
    let now = Instant::now();

    // Remove workspaces that have been idle longer than TTL.
    indexes.retain(|_, managed| now.duration_since(managed.last_access) <= config.idle_ttl);

    // If we still exceed max_indexes, evict the least-recently-used.
    while indexes.len() > config.max_indexes {
        let oldest_key = indexes
            .iter()
            .min_by_key(|(_, m)| m.last_access)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest_key {
            indexes.remove(&key);
        } else {
            break;
        }
    }
}
