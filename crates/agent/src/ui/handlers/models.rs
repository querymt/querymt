//! Model listing and selection handlers.
//!
//! - `handle_list_all_models` — fetch all models from all providers (with moka cache)
//! - `handle_get_recent_models` — recent models from ViewStore
//! - `handle_set_session_model` — update the model for a specific session
//! - `fetch_all_models` / `resolve_provider_api_key` / `resolve_base_url_for_provider` — helpers

use super::super::ServerState;
use super::super::connection::{send_error, send_message};
#[cfg(feature = "oauth")]
use super::super::messages::AuthProviderEntry;
#[cfg(feature = "oauth")]
use super::super::messages::OAuthStatus;
use super::super::messages::{ModelEntry, UiServerMessage};
#[cfg(feature = "remote")]
use futures_util::StreamExt;
use futures_util::future;
use querymt::LLMParams;
use querymt::plugin::HTTPLLMProviderFactory;
use serde_json::Value;
#[cfg(feature = "oauth")]
use std::collections::HashSet;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

// ── Provider config helpers ───────────────────────────────────────────────────

pub(super) fn resolve_base_url_for_provider(state: &ServerState, provider: &str) -> Option<String> {
    let cfg: &LLMParams = state.agent.config.provider.initial_config();
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

/// Resolve API key for a provider from OAuth token store or environment variable.
async fn resolve_provider_api_key(
    provider: &str,
    factory: &dyn HTTPLLMProviderFactory,
) -> Option<String> {
    let api_key_name = factory.api_key_name()?;
    #[cfg(feature = "oauth")]
    {
        if let Ok(token) = crate::auth::get_or_refresh_token(provider).await {
            return Some(token);
        }
    }
    std::env::var(api_key_name).ok()
}

// ── Public handlers ───────────────────────────────────────────────────────────

/// Handle OAuth provider listing for dashboard auth UI.
pub async fn handle_list_auth_providers(state: &ServerState, tx: &mpsc::Sender<String>) {
    #[cfg(feature = "oauth")]
    {
        let registry = state.agent.config.provider.plugin_registry();

        let mut seen = HashSet::new();
        let mut providers = Vec::new();

        let store = crate::auth::SecretStore::new().ok();

        for cfg in &registry.config.providers {
            if !seen.insert(cfg.name.clone()) {
                continue;
            }

            let provider_name = cfg.name.clone();
            let oauth_provider = match crate::auth::get_oauth_provider(&provider_name, None) {
                Ok(provider) => provider,
                Err(_) => continue,
            };

            let status = match store
                .as_ref()
                .and_then(|s| s.get_oauth_tokens(&provider_name))
            {
                Some(tokens) if tokens.is_expired() => OAuthStatus::Expired,
                Some(_) => OAuthStatus::Connected,
                None => OAuthStatus::NotAuthenticated,
            };

            providers.push(AuthProviderEntry {
                provider: provider_name,
                display_name: oauth_provider.display_name().to_string(),
                status,
            });
        }

        providers.sort_by(|a, b| a.provider.cmp(&b.provider));

        let _ = send_message(tx, UiServerMessage::AuthProviders { providers }).await;
    }

    #[cfg(not(feature = "oauth"))]
    {
        let _ = send_message(
            tx,
            UiServerMessage::AuthProviders {
                providers: Vec::new(),
            },
        )
        .await;
    }
}

/// Handle model listing request using moka cache.
pub async fn handle_list_all_models(state: &ServerState, refresh: bool, tx: &mpsc::Sender<String>) {
    if refresh {
        state.model_cache.invalidate(&()).await;
    }

    let result = state
        .model_cache
        .try_get_with((), fetch_all_models(state, tx))
        .await;

    match result {
        Ok(models) => {
            let _ = send_message(tx, UiServerMessage::AllModelsList { models }).await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Failed to fetch models: {}", e)).await;
        }
    }
}

/// Handle request for recent models from event history.
pub async fn handle_get_recent_models(
    state: &ServerState,
    limit_per_workspace: usize,
    tx: &mpsc::Sender<String>,
) {
    match state
        .view_store
        .get_recent_models_view(limit_per_workspace)
        .await
    {
        Ok(view) => {
            let by_workspace: std::collections::HashMap<
                Option<String>,
                Vec<super::super::messages::RecentModelEntry>,
            > = view
                .by_workspace
                .into_iter()
                .map(|(workspace, entries)| {
                    let converted_entries = entries
                        .into_iter()
                        .map(|entry| super::super::messages::RecentModelEntry {
                            provider: entry.provider,
                            model: entry.model,
                            last_used: entry.last_used.format(&Rfc3339).unwrap_or_default(),
                            use_count: entry.use_count,
                        })
                        .collect();
                    (workspace, converted_entries)
                })
                .collect();

            let _ = send_message(tx, UiServerMessage::RecentModels { by_workspace }).await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Failed to get recent models: {}", e)).await;
        }
    }
}

/// Handle session model change request.
///
/// `node` is `None` for local providers, `Some(hostname)` when the user selected
/// a model that lives on a remote mesh node.
///
/// When the target session is **remote** and no explicit `node` is provided
/// (i.e. the user picked a local-only model), we automatically set
/// `provider_node` to the local node's hostname so the remote `SessionActor`
/// routes the LLM call back through the mesh via `MeshChatProvider` instead
/// of trying (and failing) to resolve the provider locally on the remote node.
pub async fn handle_set_session_model(
    state: &ServerState,
    session_id: &str,
    model_id: &str,
    node: Option<&str>,
) -> Result<(), String> {
    use crate::agent::messages::SetSessionModel;

    // Look up the session actor ref through the registry so remote sessions work too.
    let session_ref = {
        let registry = state.agent.registry.lock().await;
        registry.get(session_id).cloned()
    };

    let Some(session_ref) = session_ref else {
        return Err(format!("Session not found: {}", session_id));
    };

    // When the session lives on a remote node and the user selected a local
    // model (node == None), tag the request with our own hostname so the
    // remote SessionActor will route the LLM call back to us via the mesh.
    #[cfg(feature = "remote")]
    let effective_node: Option<String> = if node.is_some() {
        node.map(|s| s.to_string())
    } else if session_ref.is_remote() {
        state
            .agent
            .mesh()
            .map(|mesh| mesh.local_hostname().to_string())
    } else {
        None
    };
    #[cfg(not(feature = "remote"))]
    let effective_node: Option<String> = node.map(|s| s.to_string());

    let req = agent_client_protocol::SetSessionModelRequest::new(
        session_id.to_string(),
        model_id.to_string(),
    );
    // Attach the provider_node field so the SessionActor can store it in LLMConfig.
    let msg = SetSessionModel {
        req,
        provider_node: effective_node,
    };

    session_ref
        .set_session_model_with_node(msg)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Fetch models from all providers in parallel (local + remote mesh nodes).
async fn fetch_all_models(
    state: &ServerState,
    tx: &mpsc::Sender<String>,
) -> Result<Vec<ModelEntry>, String> {
    // ── Local providers ───────────────────────────────────────────────────────
    let local_models = fetch_local_models(state, tx).await;

    // ── Remote mesh peers (requires `remote` feature) ─────────────────────────
    #[cfg(feature = "remote")]
    let remote_models = fetch_remote_models(state).await;
    #[cfg(not(feature = "remote"))]
    let remote_models: Vec<ModelEntry> = Vec::new();

    let mut all = local_models;
    all.extend(remote_models);
    Ok(all)
}

/// Fetch models from the local plugin registry.
async fn fetch_local_models(state: &ServerState, tx: &mpsc::Sender<String>) -> Vec<ModelEntry> {
    let registry = state.agent.config.provider.plugin_registry();
    registry.load_all_plugins().await;

    let futures: Vec<_> = registry
        .list()
        .into_iter()
        .map(|factory| {
            let tx = tx.clone();
            async move {
                let provider_name = factory.name().to_string();

                let mut cfg = if let Some(http_factory) = factory.as_http() {
                    if let Some(api_key) =
                        resolve_provider_api_key(&provider_name, http_factory).await
                    {
                        serde_json::json!({"api_key": api_key})
                    } else {
                        return Vec::new();
                    }
                } else {
                    serde_json::json!({})
                };

                if let Some(base_url) = resolve_base_url_for_provider(state, &provider_name) {
                    cfg["base_url"] = base_url.into();
                }

                let cfg_str = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
                match factory.list_models(&cfg_str).await {
                    Ok(model_list) => model_list
                        .into_iter()
                        .map(|model| ModelEntry {
                            provider: provider_name.clone(),
                            model,
                            node: None, // local
                        })
                        .collect(),
                    Err(err) => {
                        let _ = send_error(
                            &tx,
                            format!("Failed to list models for {}: {}", provider_name, err),
                        )
                        .await;
                        Vec::new()
                    }
                }
            }
        })
        .collect();

    let results: Vec<Vec<ModelEntry>> = future::join_all(futures).await;
    results.into_iter().flatten().collect()
}

/// Query all reachable mesh peers for their available models.
///
/// This is best-effort: nodes that are unreachable or return errors are logged
/// and skipped — the local model list is returned regardless.
#[cfg(feature = "remote")]
async fn fetch_remote_models(state: &ServerState) -> Vec<ModelEntry> {
    use crate::agent::remote::{GetNodeInfo, ListAvailableModels, RemoteNodeManager};

    let Some(mesh) = state.agent.mesh() else {
        return Vec::new();
    };

    let local_peer_id = *mesh.peer_id();
    let mut stream = mesh.lookup_all_actors::<RemoteNodeManager>("node_manager");
    let mut all_remote = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(node_manager_ref) => {
                // Skip local node — its models are already fetched via fetch_local_models.
                if node_manager_ref.id().peer_id() == Some(&local_peer_id) {
                    continue;
                }

                // Get the node's hostname for tagging
                let hostname = match node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo).await {
                    Ok(info) => info.hostname,
                    Err(e) => {
                        log::warn!("fetch_remote_models: GetNodeInfo failed: {}", e);
                        continue;
                    }
                };

                // Query available models
                match node_manager_ref
                    .ask::<ListAvailableModels>(&ListAvailableModels)
                    .await
                {
                    Ok(models) => {
                        log::debug!(
                            "fetch_remote_models: got {} models from node '{}'",
                            models.len(),
                            hostname
                        );
                        for m in models {
                            all_remote.push(ModelEntry {
                                provider: m.provider,
                                model: m.model,
                                node: Some(hostname.clone()),
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "fetch_remote_models: ListAvailableModels failed for '{}': {}",
                            hostname,
                            e
                        );
                    }
                }
            }
            Err(e) => log::warn!("fetch_remote_models: lookup error: {}", e),
        }
    }

    all_remote
}
