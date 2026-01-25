//! Message handlers for UI client requests.
//!
//! Contains all `handle_*` functions that process incoming UI client messages.

use super::ServerState;
use super::connection::{send_error, send_message, send_state};
use super::mentions::filter_index_for_cwd;
use super::messages::{ModelEntry, SessionGroup, SessionSummary, UiClientMessage, UiServerMessage};
use super::session::{PRIMARY_AGENT_ID, ensure_sessions_for_mode, prompt_for_mode, resolve_cwd};
use crate::index::resolve_workspace_root;
use crate::send_agent::SendAgent;
use agent_client_protocol::CancelNotification;
use querymt::plugin::HTTPLLMProviderFactory;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

/// Main message dispatch handler.
pub async fn handle_ui_message(
    state: &ServerState,
    conn_id: &str,
    tx: &mpsc::Sender<String>,
    msg: UiClientMessage,
) {
    match msg {
        UiClientMessage::Init => {
            send_state(state, conn_id, tx).await;
            handle_list_sessions(state, tx).await;
        }
        UiClientMessage::SetActiveAgent { agent_id } => {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.active_agent_id = agent_id;
            }
            drop(connections);
            send_state(state, conn_id, tx).await;
        }
        UiClientMessage::SetRoutingMode { mode } => {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.routing_mode = mode;
            }
            drop(connections);
            send_state(state, conn_id, tx).await;
        }
        UiClientMessage::NewSession { cwd } => {
            let cwd = resolve_cwd(cwd);

            // Clear existing sessions for this connection to start fresh
            {
                let mut connections = state.connections.lock().await;
                if let Some(conn) = connections.get_mut(conn_id) {
                    let session_ids: Vec<String> = conn.sessions.values().cloned().collect();
                    conn.sessions.clear();

                    drop(connections);

                    // Clean up ownership maps
                    let mut owners = state.session_owners.lock().await;
                    let mut agents = state.session_agents.lock().await;
                    let mut cwds = state.session_cwds.lock().await;
                    for sid in session_ids {
                        owners.remove(&sid);
                        agents.remove(&sid);
                        cwds.remove(&sid);
                    }
                }
            }

            if let Err(err) = ensure_sessions_for_mode(state, conn_id, cwd.as_ref(), tx).await {
                let _ = send_error(tx, err).await;
            }

            // Auto-refresh session list after creating new session
            handle_list_sessions(state, tx).await;
        }
        UiClientMessage::Prompt { text } => {
            if text.trim().is_empty() {
                return;
            }
            let cwd = resolve_cwd(None);
            if let Err(err) = prompt_for_mode(state, conn_id, &text, cwd.as_ref(), tx).await {
                let _ = send_error(tx, err).await;
            }
        }
        UiClientMessage::ListSessions => {
            handle_list_sessions(state, tx).await;
        }
        UiClientMessage::LoadSession { session_id } => {
            handle_load_session(state, conn_id, &session_id, tx).await;
        }
        UiClientMessage::ListAllModels { refresh } => {
            handle_list_all_models(state, refresh, tx).await;
        }
        UiClientMessage::SetSessionModel {
            session_id,
            model_id,
        } => {
            if let Err(err) = handle_set_session_model(state, &session_id, &model_id).await {
                let _ = send_error(tx, err).await;
            }
        }
        UiClientMessage::GetFileIndex => {
            handle_get_file_index(state, conn_id, tx).await;
        }
        UiClientMessage::GetLlmConfig { config_id } => {
            handle_get_llm_config(state, config_id, tx).await;
        }
        UiClientMessage::CancelSession => {
            handle_cancel_session(state, conn_id, tx).await;
        }
    }
}

/// Handle session listing request.
pub async fn handle_list_sessions(state: &ServerState, tx: &mpsc::Sender<String>) {
    // Use ViewStore to get pre-grouped session list
    let view = match state.view_store.get_session_list_view(None).await {
        Ok(view) => view,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list sessions: {}", e)).await;
            return;
        }
    };

    // Convert to UI message format
    let groups: Vec<SessionGroup> = view
        .groups
        .into_iter()
        .map(|g| SessionGroup {
            cwd: g.cwd,
            latest_activity: g.latest_activity.and_then(|t| t.format(&Rfc3339).ok()),
            sessions: g
                .sessions
                .into_iter()
                .map(|s| SessionSummary {
                    session_id: s.session_id,
                    name: s.name,
                    cwd: s.cwd,
                    title: s.title,
                    created_at: s.created_at.and_then(|t| t.format(&Rfc3339).ok()),
                    updated_at: s.updated_at.and_then(|t| t.format(&Rfc3339).ok()),
                })
                .collect(),
        })
        .collect();

    let _ = send_message(tx, UiServerMessage::SessionList { groups }).await;
}

/// Handle session loading request.
pub async fn handle_load_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    // 1. Get full audit view (includes events, tasks, decisions, artifacts, etc.)
    let audit = match state.view_store.get_audit_view(session_id).await {
        Ok(audit) => audit,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to load session: {}", e)).await;
            return;
        }
    };

    // 1a. Load session to get cwd and populate session_cwds
    if let Ok(Some(session)) = state.agent.provider.get_session(session_id).await
        && let Some(cwd) = session.cwd
    {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.to_string(), cwd);
    }

    // 2. Register session ownership
    {
        let mut owners = state.session_owners.lock().await;
        owners.insert(session_id.to_string(), conn_id.to_string());
    }

    // 3. Determine agent ID (default to primary)
    let agent_id = PRIMARY_AGENT_ID.to_string();

    // 4. Register in connection state
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.clone(), session_id.to_string());
        }
    }

    // 5. Register agent mapping
    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id);
    }

    // 6. Send loaded audit view
    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            audit,
        },
    )
    .await;

    // 7. Send updated state
    send_state(state, conn_id, tx).await;
}

/// Handle model listing request using moka cache.
pub async fn handle_list_all_models(state: &ServerState, refresh: bool, tx: &mpsc::Sender<String>) {
    // Invalidate cache if refresh requested
    if refresh {
        state.model_cache.invalidate(&()).await;
    }

    // Try to get from cache, or fetch if not present
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

/// Fetch models from all providers.
async fn fetch_all_models(
    state: &ServerState,
    tx: &mpsc::Sender<String>,
) -> Result<Vec<ModelEntry>, String> {
    let registry = state.agent.provider.plugin_registry();
    registry.load_all_plugins().await;

    let mut models: Vec<ModelEntry> = Vec::new();

    for factory in registry.list() {
        let provider_name = factory.name().to_string();

        // Build config with API key (same as CLI)
        let cfg = if let Some(http_factory) = factory.as_http() {
            if let Some(api_key) = resolve_provider_api_key(&provider_name, http_factory).await {
                serde_json::json!({"api_key": api_key})
            } else {
                serde_json::json!({})
            }
        } else {
            serde_json::json!({})
        };

        // Fetch models; on error, log and continue
        match factory.list_models(&cfg).await {
            Ok(model_list) => {
                for model in model_list {
                    models.push(ModelEntry {
                        provider: provider_name.clone(),
                        model,
                    });
                }
            }
            Err(err) => {
                // Send error to system log but continue with other providers
                let _ = send_error(
                    tx,
                    format!("Failed to list models for {}: {}", provider_name, err),
                )
                .await;
            }
        }
    }

    Ok(models)
}

/// Handle session model change request.
pub async fn handle_set_session_model(
    state: &ServerState,
    session_id: &str,
    model_id: &str,
) -> Result<(), String> {
    let (provider, model) = if let Some((provider, model)) = model_id.split_once('/') {
        (provider.to_string(), model.to_string())
    } else {
        return Err("model_id must be in provider/model format".to_string());
    };

    state
        .agent
        .set_provider(session_id, &provider, &model)
        .await
        .map_err(|err: agent_client_protocol::Error| err.to_string())?;
    Ok(())
}

/// Handle file index request.
pub async fn handle_get_file_index(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let Some(session_id) = session_id else {
        let _ = send_error(tx, "No active session".to_string()).await;
        return;
    };

    let cwd = {
        let cwds = state.session_cwds.lock().await;
        cwds.get(&session_id).cloned()
    };

    let Some(cwd) = cwd else {
        let _ = send_error(tx, "No working directory set for this session".to_string()).await;
        return;
    };

    let root = resolve_workspace_root(&cwd);

    let workspace = match state.workspace_manager.get_or_create(root.clone()).await {
        Ok(workspace) => workspace,
        Err(err) => {
            let _ = send_error(tx, format!("Workspace index error: {}", err)).await;
            return;
        }
    };

    let Some(index) = workspace.file_index() else {
        let _ = send_error(tx, "File index not ready".to_string()).await;
        return;
    };

    let relative_cwd = match cwd.strip_prefix(&root) {
        Ok(relative) => relative,
        Err(_) => {
            let _ = send_error(tx, "Working directory outside workspace root".to_string()).await;
            return;
        }
    };

    let files = filter_index_for_cwd(&index, relative_cwd);

    let _ = send_message(
        tx,
        UiServerMessage::FileIndex {
            files,
            generated_at: index.generated_at,
        },
    )
    .await;
}

/// Handle LLM config request.
pub async fn handle_get_llm_config(state: &ServerState, config_id: i64, tx: &mpsc::Sender<String>) {
    match state.agent.get_llm_config(config_id).await {
        Ok(Some(config)) => {
            let _ = send_message(
                tx,
                UiServerMessage::LlmConfig {
                    config_id: config.id,
                    provider: config.provider,
                    model: config.model,
                    params: config.params,
                },
            )
            .await;
        }
        Ok(None) => {
            let _ = send_error(tx, format!("LLM config not found: {}", config_id)).await;
        }
        Err(err) => {
            let _ = send_error(tx, format!("Failed to get LLM config: {}", err)).await;
        }
    }
}

/// Handle session cancellation request.
pub async fn handle_cancel_session(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    // Get active session for this connection's active agent
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let Some(session_id) = session_id else {
        let _ = send_error(tx, "No active session to cancel".to_string()).await;
        return;
    };

    // Call cancel on the agent
    let notif = CancelNotification::new(session_id);
    if let Err(e) = state.agent.cancel(notif).await {
        let _ = send_error(tx, format!("Failed to cancel session: {}", e)).await;
    }
}

/// Resolve API key for a provider from OAuth or environment.
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
