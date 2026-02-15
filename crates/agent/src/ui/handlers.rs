//! Message handlers for UI client requests.
//!
//! Contains all `handle_*` functions that process incoming UI client messages.

#[cfg(feature = "oauth")]
use super::PendingOAuthFlow;
use super::ServerState;
use super::connection::{send_error, send_message, send_state};
use super::mentions::filter_index_for_cwd;
#[cfg(feature = "oauth")]
use super::messages::{AuthProviderEntry, OAuthStatus};
use super::messages::{ModelEntry, SessionGroup, SessionSummary, UiClientMessage, UiServerMessage};
use super::session::{PRIMARY_AGENT_ID, ensure_sessions_for_mode, prompt_for_mode, resolve_cwd};
use crate::agent::core::AgentMode;
use crate::index::resolve_workspace_root;
use agent_client_protocol::CancelNotification;
#[cfg(feature = "oauth")]
use axum::{
    Router,
    extract::{Query, State},
    response::Html,
    routing::get,
};
use futures_util::future;
use querymt::LLMParams;
use querymt::plugin::HTTPLLMProviderFactory;
#[cfg(feature = "oauth")]
use serde::Deserialize;
use serde_json::Value;
#[cfg(feature = "oauth")]
use std::collections::HashSet;
#[cfg(feature = "oauth")]
use std::sync::Arc;
#[cfg(feature = "oauth")]
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
#[cfg(feature = "oauth")]
use tokio::sync::{Mutex as AsyncMutex, oneshot};
#[cfg(feature = "oauth")]
use uuid::Uuid;

#[cfg(feature = "oauth")]
const OAUTH_FLOW_TTL_SECS: u64 = 15 * 60;

#[cfg(feature = "oauth")]
const OAUTH_CALLBACK_TIMEOUT_SECS: u64 = 5 * 60;

#[cfg(feature = "oauth")]
const OAUTH_CALLBACK_BIND_ADDR: &str = "127.0.0.1:1455";

#[cfg(feature = "oauth")]
#[derive(Debug)]
struct OAuthCallbackPayload {
    code: String,
    state: String,
}

#[cfg(feature = "oauth")]
type OAuthCallbackResult = Result<OAuthCallbackPayload, String>;

#[cfg(feature = "oauth")]
type OAuthCallbackResultSender = oneshot::Sender<OAuthCallbackResult>;

#[cfg(feature = "oauth")]
type SharedOAuthCallbackResultSender = Arc<AsyncMutex<Option<OAuthCallbackResultSender>>>;

#[cfg(feature = "oauth")]
#[derive(Debug, Deserialize)]
struct OAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[cfg(feature = "oauth")]
#[derive(Clone)]
struct OAuthCallbackHttpState {
    expected_state: String,
    result_tx: SharedOAuthCallbackResultSender,
}

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
        UiClientMessage::NewSession { cwd, request_id } => {
            let cwd = resolve_cwd(cwd).or_else(|| state.default_cwd.clone());

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

            if let Err(err) =
                ensure_sessions_for_mode(state, conn_id, cwd.as_ref(), tx, request_id.as_deref())
                    .await
            {
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
                // Agent errors are already emitted via AgentEventKind::Error and sent through
                // the event stream, so we don't need to call send_error() here to avoid duplicates.
                let _ = prompt_for_mode(&state, &conn_id, &text, cwd.as_ref(), &tx).await;
                // Refresh session list after prompt completes so titles are up to date
                handle_list_sessions(&state, &tx).await;
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
        UiClientMessage::GetRecentModels {
            limit_per_workspace,
        } => {
            let limit = limit_per_workspace.unwrap_or(10) as usize;
            handle_get_recent_models(state, limit, tx).await;
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
        UiClientMessage::ElicitationResponse {
            elicitation_id,
            action,
            content,
        } => {
            handle_elicitation_response(state, &elicitation_id, &action, content.as_ref()).await;
        }
        UiClientMessage::ListAuthProviders => {
            handle_list_auth_providers(state, tx).await;
        }
        UiClientMessage::StartOAuthLogin { provider } => {
            handle_start_oauth_login(state, conn_id, &provider, tx).await;
        }
        UiClientMessage::CompleteOAuthLogin { flow_id, response } => {
            handle_complete_oauth_login(state, conn_id, &flow_id, &response, tx).await;
        }
        UiClientMessage::DisconnectOAuth { provider } => {
            handle_disconnect_oauth(state, conn_id, &provider, tx).await;
        }
        UiClientMessage::SetAgentMode { mode } => {
            handle_set_agent_mode(state, &mode, tx).await;
        }
        UiClientMessage::GetAgentMode => {
            handle_get_agent_mode(state, tx).await;
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
    let cwd_path = if let Ok(Some(session)) = state.agent.provider.get_session(session_id).await
        && let Some(cwd) = session.cwd
    {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.to_string(), cwd.clone());
        Some(cwd)
    } else {
        None
    };

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

    // 7. Subscribe to file index updates if this session has a cwd
    if let Some(cwd) = cwd_path {
        let root = resolve_workspace_root(&cwd);
        super::connection::subscribe_to_file_index(
            state.clone(),
            conn_id.to_string(),
            tx.clone(),
            root,
        )
        .await;
    }
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
            // Convert OffsetDateTime to RFC3339 strings for JSON serialization
            let by_workspace: std::collections::HashMap<
                Option<String>,
                Vec<super::messages::RecentModelEntry>,
            > = view
                .by_workspace
                .into_iter()
                .map(|(workspace, entries)| {
                    let converted_entries = entries
                        .into_iter()
                        .map(|entry| super::messages::RecentModelEntry {
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

/// Handle OAuth provider listing for dashboard auth UI.
pub async fn handle_list_auth_providers(state: &ServerState, tx: &mpsc::Sender<String>) {
    #[cfg(feature = "oauth")]
    {
        let registry = state.agent.provider.plugin_registry();

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

/// Start OAuth login flow for a provider.
pub async fn handle_start_oauth_login(
    state: &ServerState,
    conn_id: &str,
    provider: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "oauth")]
    {
        let provider_name = provider.trim().to_lowercase();
        if provider_name.is_empty() {
            let _ = send_error(tx, "Provider name is required".to_string()).await;
            return;
        }

        let registry = state.agent.provider.plugin_registry();
        let is_configured = registry
            .config
            .providers
            .iter()
            .any(|cfg| cfg.name == provider_name.as_str());
        if !is_configured {
            let _ = send_error(
                tx,
                format!("Provider '{}' is not configured", provider_name),
            )
            .await;
            return;
        }

        let mode = if provider_name == "anthropic" {
            Some("max")
        } else {
            None
        };

        let oauth_provider = match crate::auth::get_oauth_provider(&provider_name, mode) {
            Ok(provider) => provider,
            Err(err) => {
                let _ = send_error(tx, err.to_string()).await;
                return;
            }
        };

        let flow = match oauth_provider.start_flow().await {
            Ok(flow) => flow,
            Err(err) => {
                let _ = send_error(
                    tx,
                    format!("Failed to start OAuth flow for {}: {}", provider_name, err),
                )
                .await;
                return;
            }
        };

        let flow_id = Uuid::now_v7().to_string();
        let flow_state = flow.state.clone();
        let flow_verifier = flow.verifier.clone();
        {
            let mut flows = state.oauth_flows.lock().await;
            flows.insert(
                flow_id.clone(),
                PendingOAuthFlow {
                    conn_id: conn_id.to_string(),
                    provider: provider_name.clone(),
                    state: flow_state,
                    verifier: flow_verifier,
                    created_at: std::time::Instant::now(),
                },
            );
        }

        let _ = send_message(
            tx,
            UiServerMessage::OAuthFlowStarted {
                flow_id: flow_id.clone(),
                provider: provider_name.clone(),
                authorization_url: flow.authorization_url,
            },
        )
        .await;

        maybe_spawn_oauth_callback_listener(
            state.clone(),
            tx.clone(),
            conn_id.to_string(),
            flow_id,
            provider_name,
        )
        .await;
    }

    #[cfg(not(feature = "oauth"))]
    {
        let _ = send_error(tx, "OAuth support is not enabled in this build".to_string()).await;
    }
}

#[cfg(feature = "oauth")]
async fn maybe_spawn_oauth_callback_listener(
    state: ServerState,
    tx: mpsc::Sender<String>,
    conn_id: String,
    flow_id: String,
    provider: String,
) {
    if provider != "codex" && provider != "anthropic" {
        return;
    }

    let previous_listener = {
        let active_listener = state.oauth_callback_listener.lock().await;
        active_listener.as_ref().map(|listener| {
            (
                listener.flow_id.clone(),
                listener.provider.clone(),
                listener.conn_id.clone(),
            )
        })
    };

    if let Some((old_flow_id, old_provider, old_conn_id)) = previous_listener {
        log::debug!(
            "Restarting OAuth callback listener: replacing flow '{}' (provider='{}', conn='{}') with flow '{}' (provider='{}', conn='{}')",
            old_flow_id,
            old_provider,
            old_conn_id,
            flow_id,
            provider,
            conn_id
        );
    } else {
        log::debug!(
            "Starting OAuth callback listener for flow '{}' (provider='{}', conn='{}')",
            flow_id,
            provider,
            conn_id
        );
    }

    stop_active_oauth_callback_listener(&state, true).await;

    let (stop_tx, stop_rx) = oneshot::channel();
    let listener_state = state.clone();
    let listener_tx = tx.clone();
    let listener_conn_id = conn_id.clone();
    let listener_flow_id = flow_id.clone();
    let listener_provider = provider.clone();
    let task = tokio::spawn(async move {
        run_oauth_callback_listener_task(
            listener_state,
            listener_tx,
            listener_conn_id,
            listener_flow_id,
            listener_provider,
            stop_rx,
        )
        .await;
    });

    let mut active_listener = state.oauth_callback_listener.lock().await;
    *active_listener = Some(super::ActiveOAuthCallbackListener {
        flow_id,
        conn_id,
        provider,
        stop_tx,
        task,
    });
}

#[cfg(feature = "oauth")]
async fn stop_active_oauth_callback_listener(state: &ServerState, remove_flow: bool) {
    let active = {
        let mut active_listener = state.oauth_callback_listener.lock().await;
        active_listener.take()
    };

    if let Some(active) = active {
        if remove_flow {
            let mut flows = state.oauth_flows.lock().await;
            flows.remove(&active.flow_id);
        }

        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}

#[cfg(feature = "oauth")]
async fn stop_active_oauth_callback_listener_for_flow(
    state: &ServerState,
    flow_id: &str,
    remove_flow: bool,
) {
    let active = {
        let mut active_listener = state.oauth_callback_listener.lock().await;
        if active_listener
            .as_ref()
            .is_some_and(|listener| listener.flow_id == flow_id)
        {
            active_listener.take()
        } else {
            None
        }
    };

    if let Some(active) = active {
        if remove_flow {
            let mut flows = state.oauth_flows.lock().await;
            flows.remove(&active.flow_id);
        }

        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}

#[cfg(feature = "oauth")]
async fn stop_active_oauth_callback_listener_for_connection(state: &ServerState, conn_id: &str) {
    let active = {
        let mut active_listener = state.oauth_callback_listener.lock().await;
        if active_listener
            .as_ref()
            .is_some_and(|listener| listener.conn_id == conn_id)
        {
            active_listener.take()
        } else {
            None
        }
    };

    if let Some(active) = active {
        let mut flows = state.oauth_flows.lock().await;
        flows.remove(&active.flow_id);
        drop(flows);

        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}

#[cfg(feature = "oauth")]
async fn stop_active_oauth_callback_listener_for_connection_provider(
    state: &ServerState,
    conn_id: &str,
    provider: &str,
) {
    let active = {
        let mut active_listener = state.oauth_callback_listener.lock().await;
        if active_listener
            .as_ref()
            .is_some_and(|listener| listener.conn_id == conn_id && listener.provider == provider)
        {
            active_listener.take()
        } else {
            None
        }
    };

    if let Some(active) = active {
        let mut flows = state.oauth_flows.lock().await;
        flows.remove(&active.flow_id);
        drop(flows);

        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}

#[cfg(not(feature = "oauth"))]
pub(crate) async fn stop_oauth_callback_listener_for_connection(
    _state: &ServerState,
    _conn_id: &str,
) {
}

#[cfg(feature = "oauth")]
pub(crate) async fn stop_oauth_callback_listener_for_connection(
    state: &ServerState,
    conn_id: &str,
) {
    stop_active_oauth_callback_listener_for_connection(state, conn_id).await;
}

#[cfg(feature = "oauth")]
async fn run_oauth_callback_listener_task(
    state: ServerState,
    tx: mpsc::Sender<String>,
    conn_id: String,
    flow_id: String,
    provider: String,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let (state_param, verifier) = {
        let flows = state.oauth_flows.lock().await;
        let Some(flow) = flows.get(&flow_id) else {
            return;
        };
        if flow.conn_id != conn_id || flow.provider != provider {
            return;
        }
        (flow.state.clone(), flow.verifier.clone())
    };

    let callback_state = {
        let (result_tx, result_rx) = oneshot::channel();
        (
            OAuthCallbackHttpState {
                expected_state: state_param.clone(),
                result_tx: Arc::new(AsyncMutex::new(Some(result_tx))),
            },
            result_rx,
        )
    };
    let (http_state, callback_rx) = callback_state;

    let app = Router::new()
        .route("/auth/callback", get(oauth_callback_http_handler))
        .route("/callback", get(oauth_callback_http_handler))
        .with_state(http_state);

    let listener = match tokio::net::TcpListener::bind(OAUTH_CALLBACK_BIND_ADDR).await {
        Ok(listener) => listener,
        Err(err) => {
            log::debug!(
                "OAuth callback listener ended for flow '{}' (provider='{}'): Failed to bind {}: {}",
                flow_id,
                provider,
                OAUTH_CALLBACK_BIND_ADDR,
                err
            );
            return;
        }
    };

    log::debug!(
        "OAuth callback listener started for flow '{}' (provider='{}') on {}",
        flow_id,
        provider,
        OAUTH_CALLBACK_BIND_ADDR
    );

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    enum CallbackWaitResult {
        Stopped,
        Timeout,
        Callback(OAuthCallbackResult),
    }

    let wait_result = tokio::select! {
        _ = &mut stop_rx => CallbackWaitResult::Stopped,
        result = tokio::time::timeout(Duration::from_secs(OAUTH_CALLBACK_TIMEOUT_SECS), callback_rx) => {
            match result {
                Ok(Ok(callback)) => CallbackWaitResult::Callback(callback),
                Ok(Err(_)) => CallbackWaitResult::Callback(Err("Callback listener closed unexpectedly".to_string())),
                Err(_) => CallbackWaitResult::Timeout,
            }
        }
    };

    let _ = shutdown_tx.send(());
    let _ = server_task.await;

    let callback_payload = match wait_result {
        CallbackWaitResult::Stopped => {
            log::debug!(
                "OAuth callback listener stopped for flow '{}' (provider='{}')",
                flow_id,
                provider
            );
            return;
        }
        CallbackWaitResult::Timeout => {
            log::debug!(
                "OAuth callback listener ended for flow '{}' (provider='{}'): timeout waiting for callback",
                flow_id,
                provider
            );
            return;
        }
        CallbackWaitResult::Callback(Ok(payload)) => payload,
        CallbackWaitResult::Callback(Err(err)) => {
            log::debug!(
                "OAuth callback listener ended for flow '{}' (provider='{}'): {}",
                flow_id,
                provider,
                err
            );
            return;
        }
    };

    let flow_still_active = {
        let flows = state.oauth_flows.lock().await;
        flows
            .get(&flow_id)
            .is_some_and(|flow| flow.conn_id == conn_id && flow.provider == provider)
    };

    if !flow_still_active {
        return;
    }

    let mode = if provider == "anthropic" {
        Some("max")
    } else {
        None
    };

    let oauth_provider = match crate::auth::get_oauth_provider(&provider, mode) {
        Ok(provider_impl) => provider_impl,
        Err(err) => {
            let _ = send_message(
                &tx,
                UiServerMessage::OAuthResult {
                    provider,
                    success: false,
                    message: err.to_string(),
                },
            )
            .await;
            return;
        }
    };

    let result = async {
        let tokens = oauth_provider
            .exchange_code(&callback_payload.code, &callback_payload.state, &verifier)
            .await
            .map_err(|e| format!("Token exchange failed: {}", e))?;

        persist_oauth_credentials(oauth_provider.as_ref(), &tokens, None).await?;
        Ok::<(), String>(())
    }
    .await;

    match result {
        Ok(()) => {
            let should_emit = {
                let mut flows = state.oauth_flows.lock().await;
                flows.remove(&flow_id).is_some()
            };

            if !should_emit {
                return;
            }

            {
                let mut active_listener = state.oauth_callback_listener.lock().await;
                if active_listener
                    .as_ref()
                    .is_some_and(|listener| listener.flow_id == flow_id)
                {
                    active_listener.take();
                }
            }

            state.agent.invalidate_provider_cache().await;
            state.model_cache.invalidate(&()).await;

            let _ = send_message(
                &tx,
                UiServerMessage::OAuthResult {
                    provider: provider.clone(),
                    success: true,
                    message: format!(
                        "Successfully authenticated with {}",
                        oauth_provider.display_name()
                    ),
                },
            )
            .await;

            handle_list_auth_providers(&state, &tx).await;
            handle_list_all_models(&state, true, &tx).await;
        }
        Err(err) => {
            let _ = send_message(
                &tx,
                UiServerMessage::OAuthResult {
                    provider,
                    success: false,
                    message: err,
                },
            )
            .await;
        }
    }
}

#[cfg(feature = "oauth")]
async fn oauth_callback_http_handler(
    Query(params): Query<OAuthCallbackQuery>,
    State(state): State<OAuthCallbackHttpState>,
) -> Html<String> {
    if let Some(error) = params.error {
        send_oauth_callback_result(&state, Err(format!("OAuth error: {}", error))).await;
        return Html(
            "<html><head><title>Authorization Failed</title></head><body><h1>Authorization Failed</h1><p>OAuth returned an error.</p><p>You can close this window.</p></body></html>"
                .to_string(),
        );
    }

    let Some(code) = params.code else {
        send_oauth_callback_result(&state, Err("No authorization code received".to_string())).await;
        return Html(
            "<html><head><title>Authorization Failed</title></head><body><h1>Authorization Failed</h1><p>No authorization code received.</p><p>You can close this window.</p></body></html>"
                .to_string(),
        );
    };

    let received_state = params.state.unwrap_or_default();
    if received_state != state.expected_state {
        send_oauth_callback_result(
            &state,
            Err("State mismatch - possible CSRF attack".to_string()),
        )
        .await;
        return Html(
            "<html><head><title>Authorization Failed</title></head><body><h1>Authorization Failed</h1><p>Security validation failed.</p><p>You can close this window.</p></body></html>"
                .to_string(),
        );
    }

    send_oauth_callback_result(
        &state,
        Ok(OAuthCallbackPayload {
            code,
            state: received_state,
        }),
    )
    .await;

    Html(
        "<html><head><title>Authorization Successful</title></head><body><h1>Authorization Successful</h1><p>You can close this window and return to the dashboard.</p></body></html>"
            .to_string(),
    )
}

#[cfg(feature = "oauth")]
async fn send_oauth_callback_result(state: &OAuthCallbackHttpState, result: OAuthCallbackResult) {
    let mut result_tx = state.result_tx.lock().await;
    if let Some(tx) = result_tx.take() {
        let _ = tx.send(result);
    }
}

/// Complete OAuth login flow using callback URL or auth code pasted by user.
pub async fn handle_complete_oauth_login(
    state: &ServerState,
    conn_id: &str,
    flow_id: &str,
    response: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "oauth")]
    {
        let flow = {
            let flows = state.oauth_flows.lock().await;
            flows.get(flow_id).cloned()
        };

        let Some(flow) = flow else {
            let _ = send_error(tx, format!("Unknown OAuth flow: {}", flow_id)).await;
            return;
        };

        if flow.conn_id != conn_id {
            let _ = send_error(
                tx,
                "OAuth flow belongs to a different connection".to_string(),
            )
            .await;
            return;
        }

        if flow.created_at.elapsed().as_secs() > OAUTH_FLOW_TTL_SECS {
            {
                let mut flows = state.oauth_flows.lock().await;
                flows.remove(flow_id);
            }
            stop_active_oauth_callback_listener_for_flow(state, flow_id, false).await;
            let _ = send_message(
                tx,
                UiServerMessage::OAuthResult {
                    provider: flow.provider,
                    success: false,
                    message: "OAuth flow expired, please start again".to_string(),
                },
            )
            .await;
            return;
        }

        let code_input = response.trim();
        if code_input.is_empty() {
            let _ = send_error(tx, "Authorization response is required".to_string()).await;
            return;
        }

        let code = crate::auth::extract_code_from_query(code_input)
            .unwrap_or_else(|| code_input.to_string());

        let mode = if flow.provider == "anthropic" {
            Some("max")
        } else {
            None
        };
        let oauth_provider = match crate::auth::get_oauth_provider(&flow.provider, mode) {
            Ok(provider) => provider,
            Err(err) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::OAuthResult {
                        provider: flow.provider.clone(),
                        success: false,
                        message: err.to_string(),
                    },
                )
                .await;
                return;
            }
        };

        let result = async {
            let tokens = oauth_provider
                .exchange_code(&code, &flow.state, &flow.verifier)
                .await
                .map_err(|e| format!("Token exchange failed: {}", e))?;

            persist_oauth_credentials(oauth_provider.as_ref(), &tokens, None).await?;

            Ok::<(), String>(())
        }
        .await;

        match result {
            Ok(()) => {
                let removed = {
                    let mut flows = state.oauth_flows.lock().await;
                    flows.remove(flow_id).is_some()
                };

                if !removed {
                    return;
                }

                stop_active_oauth_callback_listener_for_flow(state, flow_id, false).await;

                state.agent.invalidate_provider_cache().await;
                state.model_cache.invalidate(&()).await;

                let _ = send_message(
                    tx,
                    UiServerMessage::OAuthResult {
                        provider: flow.provider.clone(),
                        success: true,
                        message: format!(
                            "Successfully authenticated with {}",
                            oauth_provider.display_name()
                        ),
                    },
                )
                .await;

                handle_list_auth_providers(state, tx).await;
                handle_list_all_models(state, true, tx).await;
            }
            Err(err) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::OAuthResult {
                        provider: flow.provider,
                        success: false,
                        message: err,
                    },
                )
                .await;
            }
        }
    }

    #[cfg(not(feature = "oauth"))]
    {
        let _ = send_error(tx, "OAuth support is not enabled in this build".to_string()).await;
    }
}

#[cfg(feature = "oauth")]
async fn persist_oauth_credentials(
    oauth_provider: &dyn crate::auth::OAuthProvider,
    tokens: &crate::auth::TokenSet,
    api_key_override: Option<&str>,
) -> Result<(), String> {
    let mut store = crate::auth::SecretStore::new()
        .map_err(|e| format!("Failed to access secure storage: {}", e))?;

    store
        .set_oauth_tokens(oauth_provider.name(), tokens)
        .map_err(|e| format!("Failed to save OAuth tokens: {}", e))?;

    let api_key_to_store = if let Some(api_key) = api_key_override {
        Some(api_key.to_string())
    } else {
        oauth_provider
            .create_api_key(&tokens.access_token)
            .await
            .ok()
            .flatten()
    };

    if let Some(api_key) = api_key_to_store
        && let Some(key_name) = oauth_provider.api_key_name()
        && let Err(err) = store.set(key_name, &api_key)
    {
        log::warn!(
            "Failed to persist API key for provider '{}' ({}): {}",
            oauth_provider.name(),
            key_name,
            err
        );
    }

    Ok(())
}

/// Disconnect OAuth credentials for a provider.
pub async fn handle_disconnect_oauth(
    state: &ServerState,
    conn_id: &str,
    provider: &str,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(feature = "oauth")]
    {
        let provider_name = provider.trim().to_lowercase();
        if provider_name.is_empty() {
            let _ = send_error(tx, "Provider name is required".to_string()).await;
            return;
        }

        let registry = state.agent.provider.plugin_registry();
        let is_configured = registry
            .config
            .providers
            .iter()
            .any(|cfg| cfg.name == provider_name.as_str());
        if !is_configured {
            let _ = send_error(
                tx,
                format!("Provider '{}' is not configured", provider_name),
            )
            .await;
            return;
        }

        let mode = if provider_name == "anthropic" {
            Some("max")
        } else {
            None
        };

        let oauth_provider = match crate::auth::get_oauth_provider(&provider_name, mode) {
            Ok(provider) => provider,
            Err(err) => {
                let _ = send_error(tx, err.to_string()).await;
                return;
            }
        };

        let mut store = match crate::auth::SecretStore::new() {
            Ok(store) => store,
            Err(err) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::OAuthResult {
                        provider: provider_name.clone(),
                        success: false,
                        message: format!("Failed to access secure storage: {}", err),
                    },
                )
                .await;
                return;
            }
        };

        let result = async {
            if store.get_oauth_tokens(&provider_name).is_some() {
                store
                    .delete_oauth_tokens(&provider_name)
                    .map_err(|e| format!("Failed to remove OAuth tokens: {}", e))?;
            }

            if let Some(api_key_name) = oauth_provider.api_key_name()
                && store.get(api_key_name).is_some()
            {
                store
                    .delete(api_key_name)
                    .map_err(|e| format!("Failed to remove API key '{}': {}", api_key_name, e))?;
            }

            Ok::<(), String>(())
        }
        .await;

        match result {
            Ok(()) => {
                {
                    let mut flows = state.oauth_flows.lock().await;
                    flows.retain(|_, flow| {
                        !(flow.conn_id == conn_id && flow.provider == provider_name)
                    });
                }

                stop_active_oauth_callback_listener_for_connection_provider(
                    state,
                    conn_id,
                    &provider_name,
                )
                .await;

                state.agent.invalidate_provider_cache().await;
                state.model_cache.invalidate(&()).await;

                let _ = send_message(
                    tx,
                    UiServerMessage::OAuthResult {
                        provider: provider_name,
                        success: true,
                        message: format!("Disconnected from {}", oauth_provider.display_name()),
                    },
                )
                .await;

                handle_list_auth_providers(state, tx).await;
                handle_list_all_models(state, true, tx).await;
            }
            Err(err) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::OAuthResult {
                        provider: provider_name,
                        success: false,
                        message: err,
                    },
                )
                .await;
            }
        }
    }

    #[cfg(not(feature = "oauth"))]
    {
        let _ = send_error(tx, "OAuth support is not enabled in this build".to_string()).await;
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

/// Handle elicitation response from UI client.
pub async fn handle_elicitation_response(
    state: &ServerState,
    elicitation_id: &str,
    action: &str,
    content: Option<&serde_json::Value>,
) {
    // Parse action string to enum
    let action_enum = match action {
        "accept" => crate::elicitation::ElicitationAction::Accept,
        "decline" => crate::elicitation::ElicitationAction::Decline,
        "cancel" => crate::elicitation::ElicitationAction::Cancel,
        _ => {
            log::warn!("Invalid elicitation action: {}", action);
            return;
        }
    };

    let response = crate::elicitation::ElicitationResponse {
        action: action_enum,
        content: content.cloned(),
    };

    let pending_map = state.agent.pending_elicitations();
    let mut pending = pending_map.lock().await;
    if let Some(tx) = pending.remove(elicitation_id) {
        let _ = tx.send(response);
    }
}

/// Handle agent mode change request.
pub async fn handle_set_agent_mode(state: &ServerState, mode: &str, tx: &mpsc::Sender<String>) {
    match mode.parse::<AgentMode>() {
        Ok(new_mode) => {
            let previous_mode = state.agent.get_agent_mode();
            state.agent.set_agent_mode(new_mode);

            log::info!("Agent mode changed: {} -> {}", previous_mode, new_mode);

            // Send mode confirmation back
            let _ = send_message(
                tx,
                UiServerMessage::AgentMode {
                    mode: new_mode.as_str().to_string(),
                },
            )
            .await;
        }
        Err(e) => {
            let _ = send_error(tx, e).await;
        }
    }
}

/// Handle get agent mode request.
pub async fn handle_get_agent_mode(state: &ServerState, tx: &mpsc::Sender<String>) {
    let mode = state.agent.get_agent_mode();
    let _ = send_message(
        tx,
        UiServerMessage::AgentMode {
            mode: mode.as_str().to_string(),
        },
    )
    .await;
}
