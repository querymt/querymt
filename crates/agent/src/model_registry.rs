//! Unified model registry for all model enumeration paths.
//!
//! Consolidates model listing from three previously separate implementations:
//! - UI dashboard model picker (`ui/handlers/models.rs`)
//! - ACP `querymt/models` ext method (`agent/handle.rs`)
//! - Remote node advertisement (`agent/remote/node_manager.rs`)
//!
//! Uses `moka` caches exclusively — no hand-rolled cache paths.

use futures_util::future;
use moka::future::Cache;
use querymt::plugin::{HTTPLLMProviderFactory, LLMProviderFactory};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use typeshare::typeshare;

use crate::agent::agent_config::AgentConfig;

/// TTL for local provider model listings (relatively stable).
const LOCAL_TTL: Duration = Duration::from_secs(300); // 5 min

/// TTL for remote mesh model listings (dynamic topology).
const REMOTE_TTL: Duration = Duration::from_secs(60); // 1 min

/// Cached model list entry with canonical identity.
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Canonical internal identifier (e.g., "hf:repo:file.gguf", "file:/path/to/model.gguf", or provider-specific ID)
    pub id: String,
    /// Human-readable display label
    pub label: String,
    /// Model source: "preset", "cached", "custom", "catalog"
    pub source: String,
    /// Provider name
    pub provider: String,
    /// Original model identifier (for backwards compatibility)
    pub model: String,
    /// Stable node id where this provider lives. `None` = local node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Human-readable node label for display purposes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_label: Option<String>,
    /// Model family/repo for grouping (e.g., "Qwen2.5-Coder-32B-Instruct")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Quantization level (e.g., "Q8_0", "Q6_K", "unknown")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quant: Option<String>,
}

/// Shared model registry with dual moka caches.
///
/// Used by UI, ACP, and `RemoteNodeManager` for model enumeration.
pub struct ModelRegistry {
    local_cache: Cache<(), Vec<ModelEntry>>,
    remote_cache: Cache<(), Vec<ModelEntry>>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelRegistry")
            .field("local_cache_size", &self.local_cache.entry_count())
            .field("remote_cache_size", &self.remote_cache.entry_count())
            .finish()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelRegistry {
    /// Create a new `ModelRegistry` with dual moka caches.
    pub fn new() -> Self {
        let local_cache = Cache::builder().time_to_live(LOCAL_TTL).build();
        let remote_cache = Cache::builder().time_to_live(REMOTE_TTL).build();
        Self {
            local_cache,
            remote_cache,
        }
    }

    /// Local-only list from configured providers on this node.
    pub async fn get_local_models(&self, config: &AgentConfig) -> Vec<ModelEntry> {
        let config = config.clone();
        self.local_cache
            .try_get_with((), async {
                Ok::<_, String>(enumerate_local_models(&config).await)
            })
            .await
            .unwrap_or_default()
    }

    /// Local + remote mesh models. This is the default for UI + ACP.
    ///
    /// `mesh` is `None` in local-only mode or when the remote feature is disabled.
    pub async fn get_all_models(
        &self,
        config: &AgentConfig,
        #[cfg(feature = "remote")] mesh: Option<&crate::agent::remote::MeshHandle>,
    ) -> Vec<ModelEntry> {
        let local = self.get_local_models(config).await;

        #[cfg(feature = "remote")]
        let remote = if let Some(mesh) = mesh {
            let mesh = mesh.clone();
            self.remote_cache
                .try_get_with((), async {
                    Ok::<_, String>(enumerate_remote_models(&mesh).await)
                })
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        #[cfg(not(feature = "remote"))]
        let remote: Vec<ModelEntry> = Vec::new();

        let mut all = local;
        all.extend(remote);
        all
    }

    /// Invalidate all caches.
    pub async fn invalidate_all(&self) {
        self.local_cache.invalidate(&()).await;
        self.remote_cache.invalidate(&()).await;
    }

    /// Refresh local + remote caches and return fresh aggregated list.
    pub async fn refresh_all(
        &self,
        config: &AgentConfig,
        #[cfg(feature = "remote")] mesh: Option<&crate::agent::remote::MeshHandle>,
    ) -> Vec<ModelEntry> {
        self.invalidate_all().await;

        #[cfg(feature = "remote")]
        {
            self.get_all_models(config, mesh).await
        }
        #[cfg(not(feature = "remote"))]
        {
            self.get_all_models(config).await
        }
    }
}

// ── Shared local enumerator ───────────────────────────────────────────────────

/// Shared local enumerator used by UI, ACP, and RemoteNodeManager.
pub(crate) async fn enumerate_local_models(config: &AgentConfig) -> Vec<ModelEntry> {
    let registry = config.provider.plugin_registry();
    registry.load_all_plugins().await;

    let factories = registry.list();
    let provider_names: Vec<String> = factories
        .iter()
        .map(|factory| factory.name().to_string())
        .collect();
    log::debug!(
        "enumerate_local_models: loaded {} providers: {:?}",
        provider_names.len(),
        provider_names
    );

    let futures: Vec<_> = factories
        .into_iter()
        .map(|factory| {
            let config = config.clone();
            async move {
                let provider_name = factory.name().to_string();
                let mut models = fetch_catalog_models(&config, &factory, &provider_name).await;
                let catalog_count = models.len();

                if factory.supports_custom_models() {
                    let cached = fetch_cached_gguf_models(&provider_name).await;
                    let cached_count = cached.len();
                    models.extend(cached);

                    let custom = fetch_custom_models(&config, &provider_name).await;
                    let custom_count = custom.len();
                    models.extend(custom);

                    let deduped = dedupe_models(models);
                    log::debug!(
                        "enumerate_local_models: provider='{}' supports_custom_models=true catalog={} cached={} custom={} final={}",
                        provider_name,
                        catalog_count,
                        cached_count,
                        custom_count,
                        deduped.len()
                    );
                    deduped
                } else {
                    let deduped = dedupe_models(models);
                    log::debug!(
                        "enumerate_local_models: provider='{}' supports_custom_models=false catalog={} final={}",
                        provider_name,
                        catalog_count,
                        deduped.len()
                    );
                    deduped
                }
            }
        })
        .collect();

    let results: Vec<Vec<ModelEntry>> = future::join_all(futures).await;
    let all: Vec<ModelEntry> = results.into_iter().flatten().collect();
    log::debug!(
        "enumerate_local_models: returning {} total models",
        all.len()
    );
    all
}

// ── Provider config helpers ───────────────────────────────────────────────────

pub(crate) fn resolve_base_url_for_provider(
    config: &AgentConfig,
    provider: &str,
) -> Option<String> {
    let cfg: &querymt::LLMParams = config.provider.initial_config();
    if cfg.provider.as_deref()? != provider {
        return None;
    }
    if let Some(base_url) = &cfg.base_url {
        return Some(base_url.clone());
    }
    cfg.custom
        .as_ref()
        .and_then(|m| m.get("base_url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn resolve_model_for_provider(config: &AgentConfig, provider: &str) -> Option<String> {
    let cfg: &querymt::LLMParams = config.provider.initial_config();
    if cfg.provider.as_deref()? != provider {
        return None;
    }

    cfg.model.clone().or_else(|| {
        cfg.custom
            .as_ref()
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// Resolve API key for a provider from OAuth token store, stored API key, or environment variable.
async fn resolve_provider_api_key(
    provider: &str,
    factory: &dyn HTTPLLMProviderFactory,
) -> Option<String> {
    let preferred_method = crate::session::provider::preferred_auth_method(provider);
    let mut use_oauth_resolver = false;

    crate::session::provider::resolve_api_key_with_preference(
        provider,
        factory.api_key_name().as_deref(),
        preferred_method.as_ref(),
        &mut use_oauth_resolver,
    )
    .await
}

// ── Provider-level helpers ────────────────────────────────────────────────────

async fn fetch_catalog_models(
    config: &AgentConfig,
    factory: &Arc<dyn LLMProviderFactory>,
    provider_name: &str,
) -> Vec<ModelEntry> {
    let mut cfg = if let Some(http_factory) = factory.as_http() {
        if let Some(api_key) = resolve_provider_api_key(provider_name, http_factory).await {
            serde_json::json!({"api_key": api_key})
        } else {
            return Vec::new();
        }
    } else {
        serde_json::json!({})
    };

    if let Some(base_url) = resolve_base_url_for_provider(config, provider_name) {
        cfg["base_url"] = base_url.into();
    }

    // Non-HTTP providers like llama_cpp require `model` in config even for list_models.
    if factory.as_http().is_none() {
        if let Some(model) = resolve_model_for_provider(config, provider_name) {
            cfg["model"] = model.into();
        } else {
            log::debug!(
                "fetch_catalog_models: skipping provider='{}' catalog list because no configured model was found",
                provider_name
            );
            return Vec::new();
        }
    }

    let cfg_str = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
    match factory.list_models(&cfg_str).await {
        Ok(model_list) => model_list
            .into_iter()
            .map(|model| ModelEntry {
                id: model.clone(),
                label: model.clone(),
                source: "catalog".to_string(),
                provider: provider_name.to_string(),
                model,
                node_id: None,
                node_label: None,
                family: None,
                quant: None,
            })
            .collect(),
        Err(err) => {
            log::warn!(
                "fetch_catalog_models: failed to list models for {}: {}",
                provider_name,
                err
            );
            Vec::new()
        }
    }
}

async fn fetch_cached_gguf_models(provider: &str) -> Vec<ModelEntry> {
    use querymt_provider_common::{
        canonical_id_from_hf, list_cached_hf_gguf_models, parse_gguf_metadata,
    };

    if provider != "llama_cpp" && provider != "mistralrs" {
        return Vec::new();
    }

    let cached = match list_cached_hf_gguf_models() {
        Ok(cached) => cached,
        Err(err) => {
            log::warn!(
                "fetch_cached_gguf_models: provider='{}' failed to read HF GGUF cache: {}",
                provider,
                err
            );
            return Vec::new();
        }
    };

    log::debug!(
        "fetch_cached_gguf_models: provider='{}' discovered {} cached GGUF files",
        provider,
        cached.len()
    );

    cached
        .into_iter()
        .map(|cached_model| {
            let id = canonical_id_from_hf(&cached_model.repo, &cached_model.filename);
            let metadata = parse_gguf_metadata(&cached_model.filename);
            ModelEntry {
                id: id.clone(),
                label: cached_model.filename,
                source: "cached".to_string(),
                provider: provider.to_string(),
                model: id,
                node_id: None,
                node_label: None,
                family: Some(metadata.family),
                quant: Some(metadata.quant),
            }
        })
        .collect()
}

async fn fetch_custom_models(config: &AgentConfig, provider: &str) -> Vec<ModelEntry> {
    let store = config.provider.history_store();
    let Ok(custom_models) = store.list_custom_models(provider).await else {
        return Vec::new();
    };

    custom_models
        .into_iter()
        .map(|m| {
            let model = m
                .config_json
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| m.model_id.clone());
            ModelEntry {
                id: m.model_id,
                label: m.display_name,
                source: "custom".to_string(),
                provider: m.provider,
                model,
                node_id: None,
                node_label: None,
                family: m.family,
                quant: m.quant,
            }
        })
        .collect()
}

/// Dedupe models with source-priority: custom > cached > catalog > preset.
fn dedupe_models(models: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for source in ["custom", "cached", "catalog", "preset"] {
        for model in &models {
            if model.source == source {
                let key = format!("{}:{}", model.provider, model.id);
                if seen.insert(key) {
                    out.push(model.clone());
                }
            }
        }
    }

    out
}

// ── Remote enumerator ─────────────────────────────────────────────────────────

/// Shared remote enumerator used by UI + ACP.
#[cfg(feature = "remote")]
async fn enumerate_remote_models(mesh: &crate::agent::remote::MeshHandle) -> Vec<ModelEntry> {
    use crate::agent::remote::{GetNodeInfo, ListAvailableModels, NodeId, RemoteNodeManager};
    use futures_util::StreamExt;

    let local_peer_id = *mesh.peer_id();
    let mut stream =
        mesh.lookup_all_actors::<RemoteNodeManager>(crate::agent::remote::dht_name::NODE_MANAGER);
    let mut all_remote = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(node_manager_ref) => {
                // Skip local node — its models are already fetched via enumerate_local_models.
                if node_manager_ref.id().peer_id() == Some(&local_peer_id) {
                    continue;
                }

                // Get the node's identity/label for tagging.
                let node_info = match node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo).await {
                    Ok(info) => info,
                    Err(e) => {
                        log::warn!("enumerate_remote_models: GetNodeInfo failed: {}", e);
                        continue;
                    }
                };
                if NodeId::parse(&node_info.node_id.to_string()).is_err() {
                    log::warn!(
                        "enumerate_remote_models: ignoring node with invalid id '{}'",
                        node_info.node_id
                    );
                    continue;
                }

                // Query available models
                match node_manager_ref
                    .ask::<ListAvailableModels>(&ListAvailableModels)
                    .await
                {
                    Ok(models) => {
                        log::debug!(
                            "enumerate_remote_models: got {} models from node '{}' ({})",
                            models.len(),
                            node_info.hostname,
                            node_info.node_id
                        );
                        for m in models {
                            all_remote.push(ModelEntry {
                                id: m.model.clone(),
                                label: m.model.clone(),
                                source: "catalog".to_string(),
                                provider: m.provider,
                                model: m.model,
                                node_id: Some(node_info.node_id.to_string()),
                                node_label: Some(node_info.hostname.clone()),
                                family: None,
                                quant: None,
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "enumerate_remote_models: ListAvailableModels failed for '{}' ({}): {}",
                            node_info.hostname,
                            node_info.node_id,
                            e
                        );
                    }
                }
            }
            Err(e) => log::warn!("enumerate_remote_models: lookup error: {}", e),
        }
    }

    all_remote
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_models_source_priority() {
        let models = vec![
            ModelEntry {
                id: "model-a".into(),
                label: "Model A (catalog)".into(),
                source: "catalog".into(),
                provider: "openai".into(),
                model: "model-a".into(),
                node_id: None,
                node_label: None,
                family: None,
                quant: None,
            },
            ModelEntry {
                id: "model-a".into(),
                label: "Model A (custom)".into(),
                source: "custom".into(),
                provider: "openai".into(),
                model: "model-a".into(),
                node_id: None,
                node_label: None,
                family: Some("custom-family".into()),
                quant: None,
            },
            ModelEntry {
                id: "model-b".into(),
                label: "Model B".into(),
                source: "cached".into(),
                provider: "llama_cpp".into(),
                model: "model-b".into(),
                node_id: None,
                node_label: None,
                family: None,
                quant: Some("Q8_0".into()),
            },
        ];

        let deduped = dedupe_models(models);
        assert_eq!(deduped.len(), 2);
        // custom wins over catalog for model-a
        assert_eq!(deduped[0].label, "Model A (custom)");
        assert_eq!(deduped[0].source, "custom");
        assert_eq!(deduped[1].id, "model-b");
    }

    #[test]
    fn model_entry_serialization_shape() {
        let entry = ModelEntry {
            id: "gpt-4".into(),
            label: "GPT-4".into(),
            source: "catalog".into(),
            provider: "openai".into(),
            model: "gpt-4".into(),
            node_id: None,
            node_label: None,
            family: None,
            quant: None,
        };

        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["id"], "gpt-4");
        assert_eq!(json["source"], "catalog");
        // Optional None fields should be absent
        assert!(json.get("node_id").is_none());
        assert!(json.get("family").is_none());
    }

    #[test]
    fn model_entry_with_node_serialization() {
        let entry = ModelEntry {
            id: "model-x".into(),
            label: "Model X".into(),
            source: "catalog".into(),
            provider: "anthropic".into(),
            model: "model-x".into(),
            node_id: Some("peer-abc".into()),
            node_label: Some("node-1".into()),
            family: Some("claude".into()),
            quant: None,
        };

        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["node_id"], "peer-abc");
        assert_eq!(json["node_label"], "node-1");
        assert_eq!(json["family"], "claude");
        assert!(json.get("quant").is_none());
    }

    #[tokio::test]
    async fn registry_invalidate_clears_caches() {
        let registry = ModelRegistry::new();
        // Just verify invalidation doesn't panic on empty caches
        registry.invalidate_all().await;
    }
}
