use crate::agent::agent_config::AgentConfig;
use crate::model_inventory::ModelInventory;
use querymt_remote::{
    ProviderCatalogBackend, ProviderCatalogEntry, ProviderCatalogNodeInfo, ProviderCatalogSnapshot,
};
use std::sync::Arc;

pub struct AgentProviderCatalogBackend {
    _config: Arc<AgentConfig>,
    model_inventory: ModelInventory,
    node_id: String,
    node_label: Option<String>,
}

impl AgentProviderCatalogBackend {
    pub fn new(
        config: Arc<AgentConfig>,
        model_inventory: ModelInventory,
        node_id: String,
        node_label: Option<String>,
    ) -> Self {
        Self {
            _config: config,
            model_inventory,
            node_id,
            node_label,
        }
    }
}

impl ProviderCatalogBackend for AgentProviderCatalogBackend {
    fn snapshot(&self) -> ProviderCatalogSnapshot {
        let local_entries = self.model_inventory.local_snapshot_entries_blocking();
        if local_entries.is_empty() {
            log::debug!(
                "provider catalog snapshot has 0 local models; local inventory may still be refreshing"
            );
        }
        let providers = local_entries
            .into_iter()
            .map(|entry| ProviderCatalogEntry {
                provider: entry.provider,
                model: Some(entry.model),
                label: Some(entry.label),
                family: entry.family,
                quant: entry.quant,
            })
            .collect();

        ProviderCatalogSnapshot {
            node: ProviderCatalogNodeInfo {
                node_id: self.node_id.clone(),
                node_label: self.node_label.clone(),
                capabilities: vec!["shell".to_string(), "filesystem".to_string()],
            },
            providers,
        }
    }
}
