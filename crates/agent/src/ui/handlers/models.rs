//! Model listing and selection handlers.
//!
//! - `handle_list_all_models` — fetch all models via shared `ModelRegistry`
//! - `handle_get_recent_models` — recent models from ViewStore
//! - `handle_set_session_model` — update the model for a specific session
//! - `handle_list_auth_providers` — list all providers with auth status (OAuth + API key)
//! - `handle_set_api_token` / `handle_clear_api_token` / `handle_set_auth_method` — API token management

use super::super::ServerState;
use super::super::connection::{send_error, send_message};
use super::super::messages::{
    AuthMethod, AuthProviderEntry, OAuthStatus, ProviderCapabilityEntry, UiServerMessage,
};
use super::session_ops::ensure_session_loaded;
use crate::session::store::CustomModel;
use querymt_provider_common::{
    DownloadProgress, DownloadStatus, HfModelRef, canonical_id_from_file, canonical_id_from_hf,
    download_hf_gguf_with_progress, parse_gguf_metadata,
};
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

// ── Public handlers ───────────────────────────────────────────────────────────

/// Handle provider listing for dashboard auth UI.
///
/// Delegates to the shared [`crate::auth::service::auth_status`] and maps
/// the result into the UI wire format.
pub async fn handle_list_auth_providers(state: &ServerState, tx: &mpsc::Sender<String>) {
    let statuses = state.oauth_service.auth_status(None).await;

    let providers: Vec<AuthProviderEntry> = statuses
        .into_iter()
        .map(|s| {
            let oauth_status = s.oauth_status.map(|os| match os {
                crate::auth::service::OAuthStatus::NotAuthenticated => {
                    OAuthStatus::NotAuthenticated
                }
                crate::auth::service::OAuthStatus::Expired => OAuthStatus::Expired,
                crate::auth::service::OAuthStatus::Connected => OAuthStatus::Connected,
            });
            let preferred_method = s
                .preferred_method
                .and_then(|v| v.parse::<AuthMethod>().ok());
            AuthProviderEntry {
                provider: s.provider,
                display_name: s.display_name,
                oauth_status,
                has_stored_api_key: s.has_stored_api_key,
                has_env_api_key: s.has_env_api_key,
                env_var_name: s.env_var_name,
                supports_oauth: s.supports_oauth,
                preferred_method,
            }
        })
        .collect();

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
                    state.agent.model_registry.invalidate_all().await;
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
                    state.agent.model_registry.invalidate_all().await;
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

/// Handle model listing request using the shared `ModelRegistry`.
pub async fn handle_list_all_models(state: &ServerState, refresh: bool, tx: &mpsc::Sender<String>) {
    if refresh {
        state.agent.model_registry.invalidate_all().await;
    }

    #[cfg(feature = "remote")]
    let models = state
        .agent
        .model_registry
        .get_all_models(&state.agent.config, state.agent.mesh().as_ref())
        .await;
    #[cfg(not(feature = "remote"))]
    let models = state
        .agent
        .model_registry
        .get_all_models(&state.agent.config)
        .await;

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
                state_clone.agent.model_registry.invalidate_all().await;
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
    state.agent.model_registry.invalidate_all().await;
    Ok(())
}

fn normalize_custom_model_id_for_storage<'a>(provider: &str, model_id: &'a str) -> &'a str {
    model_id
        .strip_prefix(&format!("{}/", provider))
        .unwrap_or(model_id)
}

pub async fn handle_delete_custom_model(
    state: &ServerState,
    provider: &str,
    model_id: &str,
) -> Result<(), String> {
    validate_provider_supports_custom_models(state, provider).await?;

    // Accept both canonical transport IDs ("provider/model") and legacy raw model ids.
    let bare_model_id = normalize_custom_model_id_for_storage(provider, model_id);

    state
        .session_store
        .delete_custom_model(provider, bare_model_id)
        .await
        .map_err(|e| e.to_string())?;
    state.agent.model_registry.invalidate_all().await;
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

    let req = agent_client_protocol::schema::SetSessionModelRequest::new(
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

    #[test]
    fn normalize_custom_model_id_for_storage_strips_provider_prefix() {
        let id = normalize_custom_model_id_for_storage("llama_cpp", "llama_cpp/hf:repo:model.gguf");
        assert_eq!(id, "hf:repo:model.gguf");
    }

    #[test]
    fn normalize_custom_model_id_for_storage_preserves_legacy_raw_id() {
        let id = normalize_custom_model_id_for_storage("llama_cpp", "hf:repo:model.gguf");
        assert_eq!(id, "hf:repo:model.gguf");
    }
}
