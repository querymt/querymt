use crate::profiles::{ProfileCatalog, ProfileRuntimeManager};
use notify::RecommendedWatcher;
use std::sync::Arc;

use super::AgentInfra;

pub type ProfileRuntimeHandle = Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>;

/// Public wrapper for attaching a profile catalog to an agent runtime.
///
/// Owns the filesystem watcher so live profile reloads stay active for as long
/// as the agent keeps this attachment alive.
pub struct AgentProfiles {
    manager: ProfileRuntimeHandle,
    #[allow(dead_code)]
    watcher: Option<RecommendedWatcher>,
}

impl AgentProfiles {
    pub fn new(
        catalog: Arc<dyn ProfileCatalog>,
        active_profile_id: impl Into<String>,
        infra: AgentInfra,
    ) -> Self {
        let manager: ProfileRuntimeHandle = Arc::new(ProfileRuntimeManager::with_infra_boxed(
            catalog,
            active_profile_id,
            infra,
        ));
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

    #[cfg(test)]
    pub(crate) fn has_watcher(&self) -> bool {
        self.watcher.is_some()
    }
}
