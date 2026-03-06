//! Model listing and selection handlers.
//!
//! - `handle_list_all_models` — fetch all models from all providers (with moka cache)
//! - `handle_get_recent_models` — recent models from ViewStore
//! - `handle_set_session_model` — update the model for a specific session
//! - `handle_list_auth_providers` — list all providers with auth status (OAuth + API key)
//! - `handle_set_api_token` / `handle_clear_api_token` / `handle_set_auth_method` — API token management
//! - `fetch_all_models` / `resolve_provider_api_key` / `resolve_base_url_for_provider` — helpers

use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::{
    AuthMethod, AuthProviderEntry, ModelEntry, OAuthStatus, ProviderCapabilityEntry,
    UiServerMessage,
};
use super::session_ops::ensure_session_loaded;
use crate::session::store::CustomModel;
#[cfg(feature = "remote")]
use futures_util::StreamExt;
use futures_util::future;
use querymt::LLMParams;
use querymt::plugin::{HTTPLLMProviderFactory, LLMProviderFactory};
use querymt_provider_common::{
    DownloadProgress, DownloadStatus, HfModelRef, canonical_id_from_file, canonical_id_from_hf,
    download_hf_gguf_with_progress, list_cached_hf_gguf_models, parse_gguf_metadata,
};
use serde_json::Value;
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

pub(super) fn resolve_model_for_provider(state: &ServerState, provider: &str) -> Option<String> {
    let cfg: &LLMParams = state.agent.config.provider.initial_config();
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
///
/// OAuth resolution does not require `api_key_name()` — it uses the provider name
/// to look up tokens. This allows OAuth-only providers (like Codex) that return
/// `api_key_name() = None` to still resolve credentials.
///
/// Resolution order respects the provider's preferred auth method (if configured).
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

// ── Public handlers ───────────────────────────────────────────────────────────

/// Handle provider listing for dashboard auth UI.
///
/// Lists ALL registered providers with their full auth status:
/// - OAuth connection status (if the provider supports OAuth)
/// - Whether a manually-stored API key exists in the keyring
/// - Whether the environment variable is set
/// - The user's preferred auth method
pub async fn handle_list_auth_providers(state: &ServerState, tx: &mpsc::Sender<String>) {
    let registry = state.agent.config.provider.plugin_registry();

    let mut seen = HashSet::new();
    let mut providers = Vec::new();

    let store = crate::SecretStore::new().ok();

    for cfg in &registry.config.providers {
        if !seen.insert(cfg.name.clone()) {
            continue;
        }

        let provider_name = cfg.name.clone();

        // Resolve factory to get api_key_name and display name
        let factory = registry.get(&provider_name).await;
        let env_var_name = factory
            .as_ref()
            .and_then(|f| f.as_http())
            .and_then(|h| h.api_key_name());

        // Check OAuth support
        #[cfg(feature = "oauth")]
        let (supports_oauth, oauth_status, display_name_from_oauth) = {
            match crate::auth::get_oauth_provider(&provider_name, None) {
                Ok(oauth_provider) => {
                    let status = match store
                        .as_ref()
                        .and_then(|s| s.get_oauth_tokens(&provider_name))
                    {
                        Some(tokens) if tokens.is_expired() => Some(OAuthStatus::Expired),
                        Some(_) => Some(OAuthStatus::Connected),
                        None => Some(OAuthStatus::NotAuthenticated),
                    };
                    (
                        true,
                        status,
                        Some(oauth_provider.display_name().to_string()),
                    )
                }
                Err(_) => (false, None, None),
            }
        };
        #[cfg(not(feature = "oauth"))]
        let (supports_oauth, oauth_status, display_name_from_oauth): (
            bool,
            Option<OAuthStatus>,
            Option<String>,
        ) = (false, None, None);

        // Check for stored API key in keyring
        let has_stored_api_key = env_var_name
            .as_ref()
            .and_then(|name| store.as_ref().and_then(|s| s.get(name)))
            .is_some();

        // Check for env var
        let has_env_api_key = env_var_name
            .as_ref()
            .is_some_and(|name| std::env::var(name).is_ok());

        // Get preferred auth method from store
        let preferred_method = store
            .as_ref()
            .and_then(|s| s.get(&format!("auth_method_{}", provider_name)))
            .and_then(|v| v.parse::<AuthMethod>().ok());

        // Use OAuth display name if available, otherwise capitalize the provider name
        let display_name = display_name_from_oauth.unwrap_or_else(|| {
            let mut chars = provider_name.chars();
            match chars.next() {
                None => provider_name.clone(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        });

        providers.push(AuthProviderEntry {
            provider: provider_name,
            display_name,
            oauth_status,
            has_stored_api_key,
            has_env_api_key,
            env_var_name: env_var_name.map(|s| s.to_string()),
            supports_oauth,
            preferred_method,
        });
    }

    providers.sort_by(|a, b| a.provider.cmp(&b.provider));

    let _ = send_message(tx, UiServerMessage::AuthProviders { providers }).await;
}

/// Handle setting an API token for a provider.
///
/// Stores the token in the system keyring under the provider's env var name
/// and invalidates the model cache so credentials are re-resolved.
/// Send an `ApiTokenResult` message and refresh the auth providers list.
///
/// Shared tail for `handle_set_api_token`, `handle_clear_api_token`, and
/// `handle_set_auth_method` — all three follow the same pattern of performing
/// a keyring operation, reporting the result, then refreshing the UI.
async fn finish_auth_op(
    state: &ServerState,
    provider: &str,
    result: Result<String, String>,
    tx: &mpsc::Sender<String>,
) {
    let (success, message) = match result {
        Ok(msg) => (true, msg),
        Err(msg) => (false, msg),
    };

    let _ = send_message(
        tx,
        UiServerMessage::ApiTokenResult {
            provider: provider.to_string(),
            success,
            message,
        },
    )
    .await;

    handle_list_auth_providers(state, tx).await;
}

/// Resolve the `api_key_name` for a provider from the plugin registry.
async fn resolve_env_var_name(state: &ServerState, provider: &str) -> Option<String> {
    let registry = state.agent.config.provider.plugin_registry();
    let factory = registry.get(provider).await;
    factory
        .as_ref()
        .and_then(|f| f.as_http())
        .and_then(|h| h.api_key_name())
}

pub async fn handle_set_api_token(
    state: &ServerState,
    provider: &str,
    api_key: &str,
    tx: &mpsc::Sender<String>,
) {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        finish_auth_op(state, provider, Err("API key cannot be empty".into()), tx).await;
        return;
    }

    let result = match resolve_env_var_name(state, provider).await {
        Some(name) => match crate::SecretStore::new() {
            Ok(mut store) => match store.set(&name, api_key) {
                Ok(()) => {
                    state.model_cache.invalidate(&()).await;
                    Ok(format!("API key stored for {} ({})", provider, name))
                }
                Err(e) => Err(format!("Failed to store API key: {}", e)),
            },
            Err(e) => Err(format!("Failed to open secret store: {}", e)),
        },
        None => Err(format!(
            "Provider '{}' does not have a known API key name",
            provider
        )),
    };

    finish_auth_op(state, provider, result, tx).await;
}

/// Handle clearing an API token for a provider.
pub async fn handle_clear_api_token(
    state: &ServerState,
    provider: &str,
    tx: &mpsc::Sender<String>,
) {
    let result = match resolve_env_var_name(state, provider).await {
        Some(name) => match crate::SecretStore::new() {
            Ok(mut store) => match store.delete(&name) {
                Ok(()) => {
                    state.model_cache.invalidate(&()).await;
                    Ok(format!("API key cleared for {} ({})", provider, name))
                }
                Err(e) => Err(format!("Failed to clear API key: {}", e)),
            },
            Err(e) => Err(format!("Failed to open secret store: {}", e)),
        },
        None => Err(format!(
            "Provider '{}' does not have a known API key name",
            provider
        )),
    };

    finish_auth_op(state, provider, result, tx).await;
}

/// Handle setting the preferred auth method for a provider.
pub async fn handle_set_auth_method(
    state: &ServerState,
    provider: &str,
    method: &AuthMethod,
    tx: &mpsc::Sender<String>,
) {
    let key = format!("auth_method_{}", provider);

    let result = match crate::SecretStore::new() {
        Ok(mut store) => match store.set(&key, method.to_string()) {
            Ok(()) => Ok(format!("Auth method set to '{}' for {}", method, provider)),
            Err(e) => Err(format!("Failed to set auth method: {}", e)),
        },
        Err(e) => Err(format!("Failed to open secret store: {}", e)),
    };

    finish_auth_op(state, provider, result, tx).await;
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
            let capabilities = fetch_provider_capabilities(state).await;
            let _ = send_message(
                tx,
                UiServerMessage::ProviderCapabilities {
                    providers: capabilities,
                },
            )
            .await;
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
                String,
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
                    (workspace.unwrap_or_default(), converted_entries)
                })
                .collect();

            let _ = send_message(tx, UiServerMessage::RecentModels { by_workspace }).await;
        }
        Err(e) => {
            let _ = send_error(tx, format!("Failed to get recent models: {}", e)).await;
        }
    }
}

pub async fn handle_add_custom_model_from_hf(
    state: &ServerState,
    provider: &str,
    repo: &str,
    filename: &str,
    display_name: Option<String>,
    tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    validate_provider_supports_custom_models(state, provider).await?;

    let model_id = canonical_id_from_hf(repo, filename);
    let tx_clone = tx.clone();
    let state_clone = state.clone();
    let provider_owned = provider.to_string();
    let repo_owned = repo.to_string();
    let filename_owned = filename.to_string();
    let display_name_owned = display_name;

    let _ = send_message(
        tx,
        UiServerMessage::ModelDownloadStatus {
            provider: provider_owned.clone(),
            model_id: model_id.clone(),
            status: "queued".to_string(),
            bytes_downloaded: 0,
            bytes_total: None,
            percent: None,
            speed_bps: None,
            eta_seconds: None,
            message: None,
        },
    )
    .await;

    tokio::spawn(async move {
        let repo_for_cb = repo_owned.clone();
        let filename_for_cb = filename_owned.clone();
        let provider_for_cb = provider_owned.clone();
        let model_id_for_cb = model_id.clone();
        let tx_for_cb = tx_clone.clone();

        let progress_cb = Box::new(move |p: DownloadProgress| {
            let tx_for_send = tx_for_cb.clone();
            let provider_for_send = provider_for_cb.clone();
            let model_id_for_send = model_id_for_cb.clone();
            let status = status_from_download(&p.status);
            let message = match &p.status {
                DownloadStatus::Failed(msg) => Some(msg.clone()),
                _ => None,
            };
            tokio::spawn(async move {
                let _ = send_message(
                    &tx_for_send,
                    UiServerMessage::ModelDownloadStatus {
                        provider: provider_for_send,
                        model_id: model_id_for_send,
                        status,
                        bytes_downloaded: p.bytes_downloaded,
                        bytes_total: p.bytes_total,
                        percent: p.percent,
                        speed_bps: p.speed_bps,
                        eta_seconds: p.eta_seconds,
                        message,
                    },
                )
                .await;
            });
        });

        let result = download_hf_gguf_with_progress(
            &HfModelRef {
                repo: repo_owned.clone(),
                file: filename_owned.clone(),
            },
            progress_cb,
        )
        .await;

        match result {
            Ok(path) => {
                let metadata = parse_gguf_metadata(&filename_for_cb);
                let custom = CustomModel {
                    provider: provider_owned.clone(),
                    model_id: model_id.clone(),
                    display_name: display_name_owned.unwrap_or_else(|| filename_for_cb.clone()),
                    config_json: serde_json::json!({
                        "model": model_id.clone(),
                        "path": path,
                    }),
                    source_type: "hf".to_string(),
                    source_ref: Some(format!("{}:{}", repo_for_cb, filename_for_cb)),
                    family: Some(metadata.family),
                    quant: Some(metadata.quant),
                    created_at: None,
                    updated_at: None,
                };

                if let Err(err) = state_clone.session_store.upsert_custom_model(&custom).await {
                    let _ = send_message(
                        &tx_clone,
                        UiServerMessage::ModelDownloadStatus {
                            provider: provider_owned,
                            model_id,
                            status: "failed".to_string(),
                            bytes_downloaded: 0,
                            bytes_total: None,
                            percent: None,
                            speed_bps: None,
                            eta_seconds: None,
                            message: Some(err.to_string()),
                        },
                    )
                    .await;
                    return;
                }
                state_clone.model_cache.invalidate(&()).await;
            }
            Err(err) => {
                let _ = send_message(
                    &tx_clone,
                    UiServerMessage::ModelDownloadStatus {
                        provider: provider_owned,
                        model_id,
                        status: "failed".to_string(),
                        bytes_downloaded: 0,
                        bytes_total: None,
                        percent: None,
                        speed_bps: None,
                        eta_seconds: None,
                        message: Some(err.to_string()),
                    },
                )
                .await;
            }
        }
    });

    Ok(())
}

pub async fn handle_add_custom_model_from_file(
    state: &ServerState,
    provider: &str,
    file_path: &str,
    display_name: Option<String>,
) -> Result<(), String> {
    validate_provider_supports_custom_models(state, provider).await?;

    let abs = std::fs::canonicalize(file_path)
        .map_err(|e| format!("failed to resolve file path '{}': {}", file_path, e))?;
    if abs.extension().and_then(|e| e.to_str()) != Some("gguf") {
        return Err("local custom model file must be a .gguf".to_string());
    }
    if !abs.is_file() {
        return Err(format!("path is not a file: {}", abs.display()));
    }

    let filename = abs
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "invalid model file name".to_string())?
        .to_string();
    let model_id = canonical_id_from_file(&abs);
    let metadata = parse_gguf_metadata(&filename);

    let custom = CustomModel {
        provider: provider.to_string(),
        model_id: model_id.clone(),
        display_name: display_name.unwrap_or(filename),
        config_json: serde_json::json!({
            "model": model_id,
            "path": abs,
        }),
        source_type: "local_file".to_string(),
        source_ref: Some(file_path.to_string()),
        family: Some(metadata.family),
        quant: Some(metadata.quant),
        created_at: None,
        updated_at: None,
    };

    state
        .session_store
        .upsert_custom_model(&custom)
        .await
        .map_err(|e| e.to_string())?;
    state.model_cache.invalidate(&()).await;
    Ok(())
}

pub async fn handle_delete_custom_model(
    state: &ServerState,
    provider: &str,
    model_id: &str,
) -> Result<(), String> {
    validate_provider_supports_custom_models(state, provider).await?;
    state
        .session_store
        .delete_custom_model(provider, model_id)
        .await
        .map_err(|e| e.to_string())?;
    state.model_cache.invalidate(&()).await;
    Ok(())
}

/// Handle session model change request.
///
/// `node_id` is `None` for local providers, `Some(peer_id)` when the user selected
/// a model that lives on a remote mesh node.
///
/// When the target session is **remote** and no explicit `node_id` is provided
/// (i.e. the user picked a local-only model), we automatically set
/// `provider_node_id` to the local node's peer id so the remote `SessionActor`
/// routes the LLM call back through the mesh via `MeshChatProvider` instead
/// of trying (and failing) to resolve the provider locally on the remote node.
pub async fn handle_set_session_model(
    state: &ServerState,
    session_id: &str,
    model_id: &str,
    #[cfg(feature = "remote")] node_id: Option<&str>,
    #[cfg(not(feature = "remote"))] _node_id: Option<&str>,
) -> Result<(), String> {
    use crate::agent::messages::SetSessionModel;

    ensure_session_loaded(state, session_id, "set_session_model").await?;

    // Look up the session actor ref through the registry so remote sessions work too.
    let session_ref = {
        let registry = state.agent.registry.lock().await;
        registry.get(session_id).cloned()
    };

    let Some(session_ref) = session_ref else {
        return Err(format!("Session not found: {}", session_id));
    };

    // When the session lives on a remote node and the user selected a local
    // model (node_id == None), tag the request with our own peer id so the
    // remote SessionActor will route the LLM call back to us via the mesh.
    #[cfg(feature = "remote")]
    let effective_node_id: Option<crate::agent::remote::NodeId> = if let Some(node_id) = node_id {
        Some(
            crate::agent::remote::NodeId::parse(node_id)
                .map_err(|e| format!("invalid node_id '{}': {}", node_id, e))?,
        )
    } else if session_ref.is_remote() {
        state
            .agent
            .mesh()
            .map(|mesh| crate::agent::remote::NodeId::from_peer_id(*mesh.peer_id()))
    } else {
        None
    };
    #[cfg(not(feature = "remote"))]
    let effective_node_id: Option<crate::agent::remote::NodeId> = None;

    let req = agent_client_protocol::SetSessionModelRequest::new(
        session_id.to_string(),
        model_id.to_string(),
    );
    // Attach the provider_node_id field so the SessionActor can store it in LLMConfig.
    let msg = SetSessionModel {
        req,
        provider_node_id: effective_node_id,
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

    let factories = registry.list();
    let provider_names: Vec<String> = factories
        .iter()
        .map(|factory| factory.name().to_string())
        .collect();
    log::debug!(
        "fetch_local_models: loaded {} providers: {:?}",
        provider_names.len(),
        provider_names
    );

    let futures: Vec<_> = factories
        .into_iter()
        .map(|factory| {
            let tx = tx.clone();
            let state = state.clone();
            async move {
                let provider_name = factory.name().to_string();
                let mut models = fetch_catalog_models(&state, &factory, &provider_name, &tx).await;
                let catalog_count = models.len();

                if factory.supports_custom_models() {
                    let cached = fetch_cached_gguf_models(&provider_name).await;
                    let cached_count = cached.len();
                    models.extend(cached);

                    let custom = fetch_custom_models(&state, &provider_name).await;
                    let custom_count = custom.len();
                    models.extend(custom);

                    let deduped = dedupe_models(models);
                    log::debug!(
                        "fetch_local_models: provider='{}' supports_custom_models=true catalog={} cached={} custom={} final={}",
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
                        "fetch_local_models: provider='{}' supports_custom_models=false catalog={} final={}",
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
    log::debug!("fetch_local_models: returning {} total models", all.len());
    all
}

async fn fetch_catalog_models(
    state: &ServerState,
    factory: &std::sync::Arc<dyn LLMProviderFactory>,
    provider_name: &str,
    tx: &mpsc::Sender<String>,
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

    if let Some(base_url) = resolve_base_url_for_provider(state, provider_name) {
        cfg["base_url"] = base_url.into();
    }

    // Non-HTTP providers like llama_cpp require `model` in config even for list_models.
    if factory.as_http().is_none() {
        if let Some(model) = resolve_model_for_provider(state, provider_name) {
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
            let _ = send_error(
                tx,
                format!("Failed to list models for {}: {}", provider_name, err),
            )
            .await;
            Vec::new()
        }
    }
}

async fn fetch_cached_gguf_models(provider: &str) -> Vec<ModelEntry> {
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

async fn fetch_custom_models(state: &ServerState, provider: &str) -> Vec<ModelEntry> {
    let Ok(custom_models) = state.session_store.list_custom_models(provider).await else {
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

async fn validate_provider_supports_custom_models(
    state: &ServerState,
    provider: &str,
) -> Result<(), String> {
    let registry = state.agent.config.provider.plugin_registry();
    registry.load_all_plugins().await;
    let supported = registry
        .list()
        .into_iter()
        .any(|factory| factory.name() == provider && factory.supports_custom_models());

    if supported {
        Ok(())
    } else {
        Err(format!(
            "provider '{}' does not support custom model management",
            provider
        ))
    }
}

fn status_from_download(status: &DownloadStatus) -> String {
    match status {
        DownloadStatus::Starting => "queued".to_string(),
        DownloadStatus::Downloading => "downloading".to_string(),
        DownloadStatus::Verifying | DownloadStatus::Completed => "completed".to_string(),
        DownloadStatus::Failed(_) => "failed".to_string(),
    }
}

async fn fetch_provider_capabilities(state: &ServerState) -> Vec<ProviderCapabilityEntry> {
    let registry = state.agent.config.provider.plugin_registry();
    registry.load_all_plugins().await;
    let mut providers: Vec<ProviderCapabilityEntry> = registry
        .list()
        .into_iter()
        .map(|factory| ProviderCapabilityEntry {
            provider: factory.name().to_string(),
            supports_custom_models: factory.supports_custom_models(),
        })
        .collect();
    providers.sort_by(|a, b| a.provider.cmp(&b.provider));
    providers
}

/// Query all reachable mesh peers for their available models.
///
/// This is best-effort: nodes that are unreachable or return errors are logged
/// and skipped — the local model list is returned regardless.
#[cfg(feature = "remote")]
async fn fetch_remote_models(state: &ServerState) -> Vec<ModelEntry> {
    use crate::agent::remote::{GetNodeInfo, ListAvailableModels, NodeId, RemoteNodeManager};

    let Some(mesh) = state.agent.mesh() else {
        return Vec::new();
    };

    let local_peer_id = *mesh.peer_id();
    let mut stream =
        mesh.lookup_all_actors::<RemoteNodeManager>(crate::agent::remote::dht_name::NODE_MANAGER);
    let mut all_remote = Vec::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(node_manager_ref) => {
                // Skip local node — its models are already fetched via fetch_local_models.
                if node_manager_ref.id().peer_id() == Some(&local_peer_id) {
                    continue;
                }

                // Get the node's identity/label for tagging.
                let node_info = match node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo).await {
                    Ok(info) => info,
                    Err(e) => {
                        log::warn!("fetch_remote_models: GetNodeInfo failed: {}", e);
                        continue;
                    }
                };
                if NodeId::parse(&node_info.node_id.to_string()).is_err() {
                    log::warn!(
                        "fetch_remote_models: ignoring node with invalid id '{}'",
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
                            "fetch_remote_models: got {} models from node '{}' ({})",
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
                            "fetch_remote_models: ListAvailableModels failed for '{}' ({}): {}",
                            node_info.hostname,
                            node_info.node_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::TestServerState;
    use tokio::time::Duration;

    /// Parse the next JSON message from the channel.
    async fn next_msg(rx: &mut mpsc::Receiver<String>) -> serde_json::Value {
        let raw = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout waiting for message")
            .expect("channel closed");
        serde_json::from_str(&raw).expect("invalid JSON from handler")
    }

    // ── handle_set_api_token ──────────────────────────────────────────────────

    #[tokio::test]
    async fn set_api_token_rejects_empty_key() {
        let ts = TestServerState::new().await;
        let (tx, mut rx) = ts.add_connection("c1").await;

        handle_set_api_token(&ts.state, "openai", "", &tx).await;

        // First message: ApiTokenResult with success=false
        let msg = next_msg(&mut rx).await;
        assert_eq!(msg["type"], "api_token_result");
        assert_eq!(msg["data"]["success"], false);
        assert!(
            msg["data"]["message"]
                .as_str()
                .unwrap()
                .contains("cannot be empty")
        );
    }

    #[tokio::test]
    async fn set_api_token_rejects_whitespace_only_key() {
        let ts = TestServerState::new().await;
        let (tx, mut rx) = ts.add_connection("c1").await;

        handle_set_api_token(&ts.state, "openai", "   \t\n  ", &tx).await;

        let msg = next_msg(&mut rx).await;
        assert_eq!(msg["type"], "api_token_result");
        assert_eq!(msg["data"]["success"], false);
    }

    #[tokio::test]
    async fn set_api_token_unknown_provider_returns_error() {
        let ts = TestServerState::new().await;
        let (tx, mut rx) = ts.add_connection("c1").await;

        handle_set_api_token(&ts.state, "nonexistent", "sk-test", &tx).await;

        let msg = next_msg(&mut rx).await;
        assert_eq!(msg["type"], "api_token_result");
        assert_eq!(msg["data"]["success"], false);
        assert!(
            msg["data"]["message"]
                .as_str()
                .unwrap()
                .contains("does not have a known API key name")
        );
    }

    // ── handle_clear_api_token ────────────────────────────────────────────────

    #[tokio::test]
    async fn clear_api_token_unknown_provider_returns_error() {
        let ts = TestServerState::new().await;
        let (tx, mut rx) = ts.add_connection("c1").await;

        handle_clear_api_token(&ts.state, "nonexistent", &tx).await;

        let msg = next_msg(&mut rx).await;
        assert_eq!(msg["type"], "api_token_result");
        assert_eq!(msg["data"]["success"], false);
        assert!(
            msg["data"]["message"]
                .as_str()
                .unwrap()
                .contains("does not have a known API key name")
        );
    }

    // ── handle_list_auth_providers ────────────────────────────────────────────

    #[tokio::test]
    async fn list_auth_providers_empty_registry() {
        let ts = TestServerState::new().await;
        let (tx, mut rx) = ts.add_connection("c1").await;

        handle_list_auth_providers(&ts.state, &tx).await;

        let msg = next_msg(&mut rx).await;
        assert_eq!(msg["type"], "auth_providers");
        let providers = msg["data"]["providers"].as_array().unwrap();
        assert!(providers.is_empty());
    }
}
