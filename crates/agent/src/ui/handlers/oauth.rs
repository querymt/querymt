//! OAuth flow handlers and callback listener infrastructure.
//!
//! Contains everything related to OAuth provider authentication:
//! - `handle_list_auth_providers`
//! - `handle_start_oauth_login` / `handle_complete_oauth_login` / `handle_disconnect_oauth`
//! - Background callback HTTP listener (started automatically for providers that
//!   support the redirect flow, e.g. `anthropic` and `codex`).
//! - Credential persistence helpers.

#[cfg(feature = "oauth")]
use super::super::PendingOAuthFlow;
use super::super::ServerState;
use super::super::connection::{send_error, send_message};
#[cfg(feature = "oauth")]
use super::super::messages::OAuthFlowKind;
use super::super::messages::UiServerMessage;
use super::models::{handle_list_all_models, handle_list_auth_providers};
#[cfg(feature = "oauth")]
use axum::{
    Router,
    extract::{Query, State},
    response::Html,
    routing::get,
};
#[cfg(feature = "oauth")]
use serde::Deserialize;
#[cfg(feature = "oauth")]
use std::sync::Arc;
#[cfg(feature = "oauth")]
use std::time::Duration;
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

// ── Public-facing handlers ────────────────────────────────────────────────────

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

        let registry = state.agent.config.provider.plugin_registry();
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
                flow_kind: oauth_provider.flow_kind(),
            },
        )
        .await;

        maybe_spawn_oauth_callback_listener(
            state.clone(),
            tx.clone(),
            conn_id.to_string(),
            flow_id,
            provider_name,
            oauth_provider.flow_kind(),
        )
        .await;
    }

    #[cfg(not(feature = "oauth"))]
    {
        let _ = send_error(tx, "OAuth support is not enabled in this build".to_string()).await;
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

        let code_input = response.trim();
        let code = match oauth_provider.flow_kind() {
            OAuthFlowKind::RedirectCode => {
                if code_input.is_empty() {
                    let _ = send_error(tx, "Authorization response is required".to_string()).await;
                    return;
                }
                crate::auth::extract_code_from_query(code_input)
                    .unwrap_or_else(|| code_input.to_string())
            }
            OAuthFlowKind::DevicePoll => code_input.to_string(),
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

                state.agent.config.invalidate_provider_cache().await;
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

        let registry = state.agent.config.provider.plugin_registry();
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

                state.agent.config.invalidate_provider_cache().await;
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

// ── Callback listener lifecycle ───────────────────────────────────────────────

#[cfg(feature = "oauth")]
async fn maybe_spawn_oauth_callback_listener(
    state: ServerState,
    tx: mpsc::Sender<String>,
    conn_id: String,
    flow_id: String,
    provider: String,
    flow_kind: OAuthFlowKind,
) {
    if flow_kind != OAuthFlowKind::RedirectCode {
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
    *active_listener = Some(super::super::ActiveOAuthCallbackListener {
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

/// Stop the active OAuth callback listener for a given connection (called on disconnect).
///
/// No-op when the `oauth` feature is disabled.
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

// ── Callback HTTP server ──────────────────────────────────────────────────────

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

            state.agent.config.invalidate_provider_cache().await;
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

// ── Credential persistence ────────────────────────────────────────────────────

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
