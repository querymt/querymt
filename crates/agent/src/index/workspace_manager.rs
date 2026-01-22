use super::{WorkspaceIndex, WorkspaceIndexError};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OnceCell};

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

#[derive(Clone)]
pub struct WorkspaceIndexManager {
    config: WorkspaceIndexManagerConfig,
    indexes: Arc<Mutex<HashMap<PathBuf, WorkspaceEntry>>>,
}

struct WorkspaceEntry {
    workspace: Arc<OnceCell<Arc<WorkspaceIndex>>>,
    last_access: Instant,
}

impl WorkspaceIndexManager {
    pub fn new(config: WorkspaceIndexManagerConfig) -> Self {
        Self {
            config,
            indexes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn get_or_create(
        &self,
        root: PathBuf,
    ) -> Result<Arc<WorkspaceIndex>, WorkspaceIndexError> {
        let root_key = root.clone();
        let cell = {
            let mut indexes = self.indexes.lock().await;
            if let Some(entry) = indexes.get_mut(&root_key) {
                entry.last_access = Instant::now();
                entry.workspace.clone()
            } else {
                let entry = WorkspaceEntry {
                    workspace: Arc::new(OnceCell::new()),
                    last_access: Instant::now(),
                };
                let cell = entry.workspace.clone();
                indexes.insert(root_key.clone(), entry);
                cell
            }
        };

        let workspace = cell
            .get_or_try_init(|| async move {
                log::debug!("WorkspaceIndexManager: initializing workspace {:?}", root);
                WorkspaceIndex::new(root).await.map(Arc::new)
            })
            .await?
            .clone();

        let mut indexes = self.indexes.lock().await;
        if let Some(entry) = indexes.get_mut(&root_key) {
            entry.last_access = Instant::now();
        }
        prune_indexes(&mut indexes, &self.config);
        Ok(workspace)
    }
}

fn prune_indexes(
    indexes: &mut HashMap<PathBuf, WorkspaceEntry>,
    config: &WorkspaceIndexManagerConfig,
) {
    let now = Instant::now();
    indexes.retain(|_, entry| {
        entry.workspace.get().is_none() || now.duration_since(entry.last_access) <= config.idle_ttl
    });

    let mut initialized_keys: Vec<_> = indexes
        .iter()
        .filter(|(_, entry)| entry.workspace.get().is_some())
        .map(|(key, entry)| (key.clone(), entry.last_access))
        .collect();

    while indexes.len() > config.max_indexes && !initialized_keys.is_empty() {
        if let Some((oldest_key, _)) = initialized_keys
            .iter()
            .min_by_key(|(_, last_access)| *last_access)
            .cloned()
        {
            indexes.remove(&oldest_key);
            initialized_keys.retain(|(key, _)| key != &oldest_key);
        } else {
            break;
        }
    }
}
