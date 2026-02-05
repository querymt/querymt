//! Message handlers for UI client requests.
//!
//! Contains all `handle_*` functions that process incoming UI client messages.

use super::ServerState;
use super::connection::{send_error, send_message, send_state};
use super::mentions::filter_index_for_cwd;
use super::messages::{ModelEntry, SessionGroup, SessionSummary, UiClientMessage, UiServerMessage};
use super::session::{PRIMARY_AGENT_ID, ensure_sessions_for_mode, prompt_for_mode, resolve_cwd};
use crate::index::resolve_workspace_root;
use agent_client_protocol::CancelNotification;
use futures_util::future;
use querymt::LLMParams;
use querymt::plugin::HTTPLLMProviderFactory;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

fn resolve_base_url_for_provider(state: &ServerState, provider: &str) -> Option<String> {
    let cfg: &LLMParams = state.agent.provider.initial_config();
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

                    // Clean up session metadata maps
                    let mut agents = state.session_agents.lock().await;
                    let mut cwds = state.session_cwds.lock().await;
                    for sid in session_ids {
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
            // Spawn prompt execution on a separate task so the WebSocket receive
            // loop continues processing messages (crucially, CancelSession).
            // Without this, the receive loop blocks on prompt completion and
            // cancel messages are never read until the prompt finishes.
            let state = state.clone();
            let conn_id = conn_id.to_string();
            let tx = tx.clone();
            tokio::spawn(async move {
                let cwd = resolve_cwd(None);
                if let Err(err) = prompt_for_mode(&state, &conn_id, &text, cwd.as_ref(), &tx).await
                {
                    let _ = send_error(&tx, err).await;
                }
            });
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
        UiClientMessage::Undo { message_id } => {
            handle_undo(state, conn_id, &message_id, tx).await;
        }
        UiClientMessage::Redo => {
            handle_redo(state, conn_id, tx).await;
        }
        UiClientMessage::SubscribeSession {
            session_id,
            agent_id,
        } => {
            handle_subscribe_session(state, conn_id, &session_id, agent_id.as_deref(), tx).await;
        }
        UiClientMessage::UnsubscribeSession { session_id } => {
            handle_unsubscribe_session(state, conn_id, &session_id).await;
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
                    parent_session_id: s.parent_session_id,
                    fork_origin: s.fork_origin,
                    has_children: s.has_children,
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
    // 1. Get audit view for this session only (child sessions loaded separately)
    let audit = match state.view_store.get_audit_view(session_id, false).await {
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

    // 2. Determine agent ID (default to primary)
    let agent_id = PRIMARY_AGENT_ID.to_string();

    // 3. Register in connection state
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.clone(), session_id.to_string());
            // Auto-subscribe to the loaded session
            conn.subscribed_sessions.insert(session_id.to_string());
        }
    }

    // 4. Register agent mapping
    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id.clone());
    }

    // 5. Send loaded audit view
    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            agent_id,
            audit,
        },
    )
    .await;

    // 6. Send updated state
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

/// Fetch models from all providers in parallel.
async fn fetch_all_models(
    state: &ServerState,
    tx: &mpsc::Sender<String>,
) -> Result<Vec<ModelEntry>, String> {
    let registry = state.agent.provider.plugin_registry();
    registry.load_all_plugins().await;

    // Build futures for all providers in parallel, skipping those without API keys
    let futures: Vec<_> = registry
        .list()
        .into_iter()
        .map(|factory| {
            let tx = tx.clone();
            async move {
                let provider_name = factory.name().to_string();

                // Build config with API key, skip HTTP providers without keys
                let mut cfg = if let Some(http_factory) = factory.as_http() {
                    if let Some(api_key) =
                        resolve_provider_api_key(&provider_name, http_factory).await
                    {
                        serde_json::json!({"api_key": api_key})
                    } else {
                        // No API key → skip this provider
                        return Vec::new();
                    }
                } else {
                    // Non-HTTP provider (e.g., ollama) — no key needed
                    serde_json::json!({})
                };

                if let Some(base_url) = resolve_base_url_for_provider(state, &provider_name) {
                    cfg["base_url"] = base_url.into();
                }

                // Fetch models; on error, log and return empty
                let cfg_str = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
                match factory.list_models(&cfg_str).await {
                    Ok(model_list) => model_list
                        .into_iter()
                        .map(|model| ModelEntry {
                            provider: provider_name.clone(),
                            model,
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

    // Execute all futures in parallel
    let results: Vec<Vec<ModelEntry>> = future::join_all(futures).await;
    Ok(results.into_iter().flatten().collect())
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

    // Route cancel to the agent that owns this session, not always the primary.
    // When the user is interacting with a delegate agent, state.agent (primary)
    // won't have this session in its active_sessions map.
    let agent_id = {
        let agents = state.session_agents.lock().await;
        agents.get(&session_id).cloned()
    };
    let agent = agent_id
        .and_then(|id| super::session::agent_for_id(state, &id))
        .unwrap_or_else(|| state.agent.clone());

    let notif = CancelNotification::new(session_id);
    if let Err(e) = agent.cancel(notif).await {
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

/// Handle session subscription request with replay.
pub async fn handle_subscribe_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    agent_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    // 1. Register subscription FIRST (so live events start flowing)
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.subscribed_sessions.insert(session_id.to_string());
        }
    }

    // 2. Register agent_id if provided
    if let Some(agent_id) = agent_id {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id.to_string());
    }

    // 3. Replay stored events (ViewStore has everything persisted)
    // Don't include child sessions - they are subscribed to separately
    let (events, resolved_agent_id) = match state.view_store.get_audit_view(session_id, false).await
    {
        Ok(audit) => {
            let resolved_agent_id = {
                let agents = state.session_agents.lock().await;
                agents
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| PRIMARY_AGENT_ID.to_string())
            };
            (audit.events, resolved_agent_id)
        }
        Err(_) => {
            // Session may be brand new with no stored events yet — that's OK
            (
                vec![],
                agent_id
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| PRIMARY_AGENT_ID.to_string()),
            )
        }
    };

    // 4. Send replay batch
    let _ = send_message(
        tx,
        UiServerMessage::SessionEvents {
            session_id: session_id.to_string(),
            agent_id: resolved_agent_id,
            events,
        },
    )
    .await;
}

/// Handle undo request.
pub async fn handle_undo(
    state: &ServerState,
    conn_id: &str,
    message_id: &str,
    tx: &mpsc::Sender<String>,
) {
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

    match state.agent.undo(&session_id, message_id).await {
        Ok(result) => {
            let _ = send_message(
                tx,
                UiServerMessage::UndoResult {
                    success: true,
                    message: None,
                    reverted_files: result.reverted_files,
                },
            )
            .await;
        }
        Err(e) => {
            let _ = send_message(
                tx,
                UiServerMessage::UndoResult {
                    success: false,
                    message: Some(e.to_string()),
                    reverted_files: vec![],
                },
            )
            .await;
        }
    }
}

/// Handle redo request.
pub async fn handle_redo(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
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

    match state.agent.redo(&session_id).await {
        Ok(_result) => {
            let _ = send_message(
                tx,
                UiServerMessage::RedoResult {
                    success: true,
                    message: None,
                },
            )
            .await;
        }
        Err(e) => {
            let _ = send_message(
                tx,
                UiServerMessage::RedoResult {
                    success: false,
                    message: Some(e.to_string()),
                },
            )
            .await;
        }
    }
}

/// Handle session unsubscription request.
pub async fn handle_unsubscribe_session(state: &ServerState, conn_id: &str, session_id: &str) {
    let mut connections = state.connections.lock().await;
    if let Some(conn) = connections.get_mut(conn_id) {
        conn.subscribed_sessions.remove(session_id);
    }
}
