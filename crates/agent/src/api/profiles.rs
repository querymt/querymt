use crate::profiles::{ProfileCatalog, ProfileRuntime, ProfileRuntimeManager};
use notify::RecommendedWatcher;
use std::sync::Arc;

use super::AgentInfra;

pub use crate::profile_host::ProfileRuntimeHandle;

/// Public wrapper for attaching a profile catalog to an agent runtime.
///
/// Owns the filesystem watcher so live profile reloads stay active for as long
/// as the agent keeps this attachment alive.
pub struct AgentProfiles {
    manager: Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>,
    #[allow(dead_code)]
    watcher: Option<RecommendedWatcher>,
}

impl AgentProfiles {
    pub fn new(
        catalog: Arc<dyn ProfileCatalog>,
        active_profile_id: impl Into<String>,
        infra: AgentInfra,
    ) -> Self {
        Self::new_with_dotagents_workspace(catalog, active_profile_id, infra, None)
    }

    pub fn new_with_dotagents_workspace(
        catalog: Arc<dyn ProfileCatalog>,
        active_profile_id: impl Into<String>,
        infra: AgentInfra,
        workspace: Option<std::path::PathBuf>,
    ) -> Self {
        let mut manager =
            ProfileRuntimeManager::with_infra_boxed(catalog, active_profile_id, infra);
        if let Some(workspace) = workspace {
            manager = manager.with_dotagents_workspace(workspace);
        }
        let manager = Arc::new(manager);
        let watcher = manager.start_profile_watcher();
        Self { manager, watcher }
    }

    pub async fn with_default_infra(
        catalog: Arc<dyn ProfileCatalog>,
        active_profile_id: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self::new(
            catalog,
            active_profile_id,
            AgentInfra::default_shared().await?,
        ))
    }

    pub fn manager(&self) -> ProfileRuntimeHandle {
        self.manager.clone()
    }

    /// The concrete manager, for top-crate callers that need the full
    /// `ProfileRuntime` (which owns an `Agent`).
    pub fn concrete_manager(&self) -> Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>> {
        self.manager.clone()
    }

    pub async fn active_runtime(&self) -> anyhow::Result<Arc<ProfileRuntime>> {
        self.manager.active_runtime().await
    }

    #[cfg(test)]
    pub(crate) fn has_watcher(&self) -> bool {
        self.watcher.is_some()
    }
}
