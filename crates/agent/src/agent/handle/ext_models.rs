use super::*;
use super::utils::ext_json_response;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_models(&self) -> Result<ExtResponse, Error> {
        // Return local + remote models via the ModelInventory (non-blocking snapshot).
        let (models, meta) = self.model_inventory.get_snapshot().await;

        if models.is_empty() && !meta.refresh_in_progress {
            self.model_inventory.trigger_refresh().await;
        }

        ext_json_response(&serde_json::json!({
            "models": models,
            "meta": {
                "stale": meta.is_stale,
                "refresh_in_progress": meta.refresh_in_progress,
                "remote_timeout_count": meta.remote_timeout_count,
                "remote_node_count": meta.remote_node_count,
            }
        }))
    }

    pub(super) async fn handle_ext_refresh_models(&self) -> Result<ExtResponse, Error> {
        let handle = self.model_inventory.trigger_refresh().await;
        let (models, meta) = self.model_inventory.get_snapshot().await;

        ext_json_response(&serde_json::json!({
            "models": models,
            "meta": {
                "stale": meta.is_stale,
                "refresh_in_progress": meta.refresh_in_progress,
                "remote_timeout_count": meta.remote_timeout_count,
                "remote_node_count": meta.remote_node_count,
                "refresh_trigger": handle.disposition().as_str(),
                "started_new_refresh": handle.started_new_refresh(),
                "wait_for_completion": handle.waits_for_completion(),
            }
        }))
    }

    pub(super) async fn handle_ext_model_info(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[derive(serde::Deserialize)]
        struct ModelInfoRequest {
            #[serde(default)]
            models: Vec<ModelKey>,
        }

        #[derive(serde::Deserialize)]
        struct ModelKey {
            provider: String,
            model: String,
        }

        let parsed: ModelInfoRequest = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::from(crate::error::AgentError::Serialization(e.to_string()))
        })?;

        let registry = querymt::providers::read_providers_from_cache();
        let mut info_map = serde_json::Map::new();

        match registry {
            Ok(reg) => {
                for key in &parsed.models {
                    let lookup = reg.get_model(&key.provider, &key.model);
                    let map_key = format!("{}/{}", key.provider, key.model);
                    match lookup {
                        Some(model_info) => {
                            let val =
                                serde_json::to_value(model_info).unwrap_or(serde_json::Value::Null);
                            info_map.insert(map_key, val);
                        }
                        None => {
                            info_map.insert(map_key, serde_json::Value::Null);
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to load providers registry for modelInfo: {}", e);
                for key in &parsed.models {
                    let map_key = format!("{}/{}", key.provider, key.model);
                    info_map.insert(map_key, serde_json::Value::Null);
                }
            }
        }

        ext_json_response(&serde_json::json!({ "models": info_map }))
    }
}
