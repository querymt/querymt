//! Shared OAuth / auth service layer.
//!
//! This module contains transport-agnostic business logic for OAuth flows,
//! provider status queries, and credential management. Both the UI WebSocket
//! handlers and the ACP `ext_method` handlers call into these functions so
//! that the core auth logic lives in exactly one place.

use crate::agent::agent_config::AgentConfig;
use crate::model_registry::ModelRegistry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

// ── Shared types ──────────────────────────────────────────────────────────────

/// A pending OAuth flow awaiting completion.
#[derive(Debug, Clone)]
pub struct PendingOAuthFlow {
    /// Identifies the connection that started this flow.
    pub owner: String,
    pub provider: String,
    pub state: String,
    pub verifier: String,
    pub created_at: Instant,
}

/// An active local HTTP callback listener for redirect-based OAuth flows.
pub struct ActiveOAuthCallbackListener {
    pub flow_id: String,
    pub owner: String,
    pub provider: String,
    pub stop_tx: tokio::sync::oneshot::Sender<()>,
    pub task: JoinHandle<()>,
}

/// Thread-safe map of flow_id -> PendingOAuthFlow.
pub type OAuthFlowMap = Arc<Mutex<HashMap<String, PendingOAuthFlow>>>;

/// Thread-safe slot for the single active callback listener.
pub type CallbackListenerSlot = Arc<Mutex<Option<ActiveOAuthCallbackListener>>>;

/// Optional callback invoked when auto callback-listener completion finishes.
///
/// Used by UI transport to push websocket updates when redirect-based OAuth
/// completes in the background. ACP passes `None`.
pub type AutoCompleteNotifier = Arc<dyn Fn(CompleteFlowResult) + Send + Sync>;

/// Result sent from the HTTP callback handler back to the listener task.
#[cfg(feature = "oauth")]
type CallbackResultSender =
    Arc<Mutex<Option<tokio::sync::oneshot::Sender<Result<(String, String), String>>>>>;

/// Identifies a specific OAuth flow (used internally by listener lifecycle).
#[cfg(feature = "oauth")]
struct FlowIdentity {
    flow_id: String,
    owner: String,
    provider: String,
}

/// Shared OAuth service dependencies. Cheaply cloneable (all fields are `Arc`-backed).
///
/// Groups the four shared dependencies (`AgentConfig`, `ModelRegistry`,
/// `OAuthFlowMap`, `CallbackListenerSlot`) that every auth operation needs,
/// providing a single entry point for all OAuth service methods.
#[derive(Clone)]
pub struct OAuthService {
    config: Arc<AgentConfig>,
    #[cfg_attr(not(feature = "oauth"), allow(dead_code))]
    model_registry: ModelRegistry,
    flows: OAuthFlowMap,
    listener_slot: CallbackListenerSlot,
}

impl OAuthService {
    /// Construct a new `OAuthService` from the shared dependencies.
    pub fn new(
        config: Arc<AgentConfig>,
        model_registry: ModelRegistry,
        flows: OAuthFlowMap,
        listener_slot: CallbackListenerSlot,
    ) -> Self {
        Self {
            config,
            model_registry,
            flows,
            listener_slot,
        }
    }
}

/// OAuth authentication status for a provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthStatus {
    NotAuthenticated,
    Expired,
    Connected,
}

/// Auth status entry for a single provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderStatus {
    pub provider: String,
    pub display_name: String,
    /// `None` if the provider has no OAuth support.
    pub oauth_status: Option<OAuthStatus>,
    pub has_stored_api_key: bool,
    pub has_env_api_key: bool,
    pub env_var_name: Option<String>,
    pub supports_oauth: bool,
    pub preferred_method: Option<String>,
}

/// Result of `start_flow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartFlowResult {
    pub flow_id: String,
    pub provider: String,
    pub authorization_url: String,
    pub flow_kind: String,
}

/// Result of `complete_flow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompleteFlowResult {
    pub provider: String,
    pub success: bool,
    pub message: String,
}

/// Result of `logout`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutResult {
    pub provider: String,
    pub success: bool,
    pub message: String,
}

// ── Constants ─────────────────────────────────────────────────────────────────

#[cfg(feature = "oauth")]
const OAUTH_FLOW_TTL_SECS: u64 = 15 * 60;

#[cfg(feature = "oauth")]
const OAUTH_CALLBACK_TIMEOUT_SECS: u64 = 5 * 60;

#[cfg(feature = "oauth")]
const OAUTH_CALLBACK_BIND_ADDR: &str = "127.0.0.1:1455";

// ── OAuthService methods ──────────────────────────────────────────────────────

impl OAuthService {
    // ── auth_status ───────────────────────────────────────────────────────

    /// Query authentication status for all configured providers (or a single one).
    ///
    /// This is the shared implementation behind both the UI `list_auth_providers`
    /// message and the ACP `_querymt/auth/status` extension method.
    pub async fn auth_status(&self, provider_filter: Option<&str>) -> Vec<AuthProviderStatus> {
        let registry = self.config.provider.plugin_registry();
        let store = crate::auth::SecretStore::new().ok();

        let mut seen = std::collections::HashSet::new();
        let mut providers = Vec::new();

        for cfg in &registry.config.providers {
            if !seen.insert(cfg.name.clone()) {
                continue;
            }

            let provider_name = cfg.name.clone();

            // If a filter was given, skip non-matching providers.
            if let Some(filter) = provider_filter
                && provider_name != filter
            {
                continue;
            }

            let factory = registry.get(&provider_name).await;
            let env_var_name = factory
                .as_ref()
                .and_then(|f| f.as_http())
                .and_then(|h| h.api_key_name());

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

            let has_stored_api_key = env_var_name
                .as_ref()
                .and_then(|name| store.as_ref().and_then(|s| s.get(name)))
                .is_some();

            let has_env_api_key = env_var_name
                .as_ref()
                .is_some_and(|name| std::env::var(name).is_ok());

            let preferred_method = store
                .as_ref()
                .and_then(|s| s.get(&format!("auth_method_{}", provider_name)));

            let display_name = display_name_from_oauth.unwrap_or_else(|| {
                let mut chars = provider_name.chars();
                match chars.next() {
                    None => provider_name.clone(),
                    Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                }
            });

            providers.push(AuthProviderStatus {
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
        providers
    }

    // ── start_flow ────────────────────────────────────────────────────────

    /// Start an OAuth login flow for a provider.
    ///
    /// On success the flow is stored in `flows` and a [`StartFlowResult`] is
    /// returned containing the authorization URL the user must visit.
    ///
    /// If the provider uses the redirect-code flow, a local HTTP callback
    /// listener is automatically spawned (via [`Self::spawn_callback_listener`]).
    /// On receiving the callback it will call [`Self::complete_flow`] server-side,
    /// persist credentials, and invalidate caches so ACP clients can discover
    /// completion via [`Self::auth_status`].
    pub async fn start_flow(
        &self,
        owner: &str,
        provider: &str,
        auto_complete_notifier: Option<AutoCompleteNotifier>,
    ) -> Result<StartFlowResult, String> {
        #[cfg(feature = "oauth")]
        {
            let provider_name = provider.trim().to_lowercase();
            if provider_name.is_empty() {
                return Err("Provider name is required".to_string());
            }

            validate_provider_configured(&self.config, &provider_name)?;

            let mode = oauth_mode_for_provider(&provider_name);
            let oauth_provider =
                crate::auth::get_oauth_provider(&provider_name, mode).map_err(|e| e.to_string())?;

            let flow = oauth_provider
                .start_flow()
                .await
                .map_err(|e| format!("Failed to start OAuth flow for {}: {}", provider_name, e))?;

            let flow_id = uuid::Uuid::now_v7().to_string();
            {
                let mut map = self.flows.lock().await;
                map.insert(
                    flow_id.clone(),
                    PendingOAuthFlow {
                        owner: owner.to_string(),
                        provider: provider_name.clone(),
                        state: flow.state.clone(),
                        verifier: flow.verifier.clone(),
                        created_at: Instant::now(),
                    },
                );
            }

            let flow_kind = oauth_provider.flow_kind();
            let flow_kind_str = match flow_kind {
                crate::auth::OAuthFlowKind::RedirectCode => "redirect_code",
                crate::auth::OAuthFlowKind::DevicePoll => "device_poll",
            };

            // Auto-spawn callback listener for redirect flows.
            if flow_kind == crate::auth::OAuthFlowKind::RedirectCode {
                self.spawn_callback_listener(
                    FlowIdentity {
                        flow_id: flow_id.clone(),
                        owner: owner.to_string(),
                        provider: provider_name.clone(),
                    },
                    auto_complete_notifier,
                )
                .await;
            }

            Ok(StartFlowResult {
                flow_id,
                provider: provider_name,
                authorization_url: flow.authorization_url,
                flow_kind: flow_kind_str.to_string(),
            })
        }

        #[cfg(not(feature = "oauth"))]
        {
            let _ = (owner, provider, auto_complete_notifier);
            Err("OAuth support is not enabled in this build".to_string())
        }
    }

    // ── complete_flow ────────────────────────────────────────────────────

    /// Complete an OAuth login flow by exchanging the authorization code for tokens.
    ///
    /// This is called:
    /// - By the UI when the user pastes the callback URL / code.
    /// - Internally by the callback listener when it receives the redirect.
    /// - By ACP clients via `_querymt/auth/complete`.
    pub async fn complete_flow(
        &self,
        owner: &str,
        flow_id: &str,
        response: &str,
    ) -> CompleteFlowResult {
        #[cfg(feature = "oauth")]
        {
            let flow = {
                let map = self.flows.lock().await;
                map.get(flow_id).cloned()
            };

            let Some(flow) = flow else {
                return CompleteFlowResult {
                    provider: String::new(),
                    success: false,
                    message: format!("Unknown OAuth flow: {}", flow_id),
                };
            };

            if flow.owner != owner {
                return CompleteFlowResult {
                    provider: flow.provider,
                    success: false,
                    message: "OAuth flow belongs to a different connection".to_string(),
                };
            }

            if flow.created_at.elapsed().as_secs() > OAUTH_FLOW_TTL_SECS {
                {
                    let mut map = self.flows.lock().await;
                    map.remove(flow_id);
                }
                stop_listener_for_flow(&self.listener_slot, &self.flows, flow_id, false).await;
                return CompleteFlowResult {
                    provider: flow.provider,
                    success: false,
                    message: "OAuth flow expired, please start again".to_string(),
                };
            }

            let mode = oauth_mode_for_provider(&flow.provider);
            let oauth_provider = match crate::auth::get_oauth_provider(&flow.provider, mode) {
                Ok(p) => p,
                Err(err) => {
                    return CompleteFlowResult {
                        provider: flow.provider,
                        success: false,
                        message: err.to_string(),
                    };
                }
            };

            let code_input = response.trim();
            let code = match oauth_provider.flow_kind() {
                crate::auth::OAuthFlowKind::RedirectCode => {
                    if code_input.is_empty() {
                        return CompleteFlowResult {
                            provider: flow.provider,
                            success: false,
                            message: "Authorization response is required".to_string(),
                        };
                    }
                    crate::auth::extract_code_from_query(code_input)
                        .unwrap_or_else(|| code_input.to_string())
                }
                crate::auth::OAuthFlowKind::DevicePoll => code_input.to_string(),
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
                        let mut map = self.flows.lock().await;
                        map.remove(flow_id).is_some()
                    };
                    if !removed {
                        return CompleteFlowResult {
                            provider: flow.provider,
                            success: false,
                            message: "Flow already completed".to_string(),
                        };
                    }

                    stop_listener_for_flow(&self.listener_slot, &self.flows, flow_id, false).await;
                    self.config.invalidate_provider_cache().await;
                    self.model_registry.invalidate_all().await;

                    CompleteFlowResult {
                        provider: flow.provider.clone(),
                        success: true,
                        message: format!(
                            "Successfully authenticated with {}",
                            oauth_provider.display_name()
                        ),
                    }
                }
                Err(err) => CompleteFlowResult {
                    provider: flow.provider,
                    success: false,
                    message: err,
                },
            }
        }

        #[cfg(not(feature = "oauth"))]
        {
            let _ = (owner, flow_id, response);
            CompleteFlowResult {
                provider: String::new(),
                success: false,
                message: "OAuth support is not enabled in this build".to_string(),
            }
        }
    }

    // ── logout ──────────────────────────────────────────────────────────

    /// Disconnect / logout OAuth credentials for a provider.
    pub async fn logout(&self, owner: &str, provider: &str) -> LogoutResult {
        #[cfg(feature = "oauth")]
        {
            let provider_name = provider.trim().to_lowercase();
            if provider_name.is_empty() {
                return LogoutResult {
                    provider: provider_name,
                    success: false,
                    message: "Provider name is required".to_string(),
                };
            }

            if let Err(e) = validate_provider_configured(&self.config, &provider_name) {
                return LogoutResult {
                    provider: provider_name,
                    success: false,
                    message: e,
                };
            }

            let mode = oauth_mode_for_provider(&provider_name);
            let oauth_provider = match crate::auth::get_oauth_provider(&provider_name, mode) {
                Ok(p) => p,
                Err(err) => {
                    return LogoutResult {
                        provider: provider_name,
                        success: false,
                        message: err.to_string(),
                    };
                }
            };

            let mut store = match crate::auth::SecretStore::new() {
                Ok(s) => s,
                Err(err) => {
                    return LogoutResult {
                        provider: provider_name,
                        success: false,
                        message: format!("Failed to access secure storage: {}", err),
                    };
                }
            };

            let delete_result = (|| {
                if store.get_oauth_tokens(&provider_name).is_some() {
                    store
                        .delete_oauth_tokens(&provider_name)
                        .map_err(|e| format!("Failed to remove OAuth tokens: {}", e))?;
                }
                if let Some(api_key_name) = oauth_provider.api_key_name()
                    && store.get(api_key_name).is_some()
                {
                    store.delete(api_key_name).map_err(|e| {
                        format!("Failed to remove API key '{}': {}", api_key_name, e)
                    })?;
                }
                Ok::<(), String>(())
            })();

            match delete_result {
                Ok(()) => {
                    // Clean up pending flows for this owner+provider.
                    {
                        let mut map = self.flows.lock().await;
                        map.retain(|_, f| !(f.owner == owner && f.provider == provider_name));
                    }
                    stop_listener_for_owner_provider(&self.listener_slot, owner, &provider_name)
                        .await;
                    self.config.invalidate_provider_cache().await;
                    self.model_registry.invalidate_all().await;

                    LogoutResult {
                        provider: provider_name,
                        success: true,
                        message: format!("Disconnected from {}", oauth_provider.display_name()),
                    }
                }
                Err(err) => LogoutResult {
                    provider: provider_name,
                    success: false,
                    message: err,
                },
            }
        }

        #[cfg(not(feature = "oauth"))]
        {
            let _ = (owner, provider);
            LogoutResult {
                provider: provider.to_string(),
                success: false,
                message: "OAuth support is not enabled in this build".to_string(),
            }
        }
    }

    // ── cleanup_owner ─────────────────────────────────────────────────────

    /// Clean up all OAuth state for a disconnecting owner (connection).
    ///
    /// Called when a UI WebSocket or ACP connection closes.
    pub async fn cleanup_owner(&self, owner: &str) {
        // Stop the listener if it belongs to this owner.
        stop_listener_for_owner(&self.listener_slot, owner).await;

        // Remove all pending flows for this owner.
        let mut map = self.flows.lock().await;
        map.retain(|_, f| f.owner != owner);
    }

    // ── Callback listener lifecycle ───────────────────────────────────────

    #[cfg(feature = "oauth")]
    async fn spawn_callback_listener(
        &self,
        identity: FlowIdentity,
        auto_complete_notifier: Option<AutoCompleteNotifier>,
    ) {
        let previous = {
            let active = self.listener_slot.lock().await;
            active
                .as_ref()
                .map(|l| (l.flow_id.clone(), l.provider.clone(), l.owner.clone()))
        };

        if let Some((old_flow_id, old_provider, old_owner)) = previous {
            log::debug!(
                "Restarting OAuth callback listener: replacing flow '{}' (provider='{}', owner='{}') with flow '{}' (provider='{}', owner='{}')",
                old_flow_id,
                old_provider,
                old_owner,
                identity.flow_id,
                identity.provider,
                identity.owner
            );
        } else {
            log::debug!(
                "Starting OAuth callback listener for flow '{}' (provider='{}', owner='{}')",
                identity.flow_id,
                identity.provider,
                identity.owner
            );
        }

        // Stop any existing listener first.
        stop_listener(&self.listener_slot, &self.flows, true).await;

        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let svc = self.clone();

        let flow_id = identity.flow_id.clone();
        let owner = identity.owner.clone();
        let provider = identity.provider.clone();

        let task = tokio::spawn(async move {
            run_callback_listener_task(svc, identity, auto_complete_notifier, stop_rx).await;
        });

        let mut active = self.listener_slot.lock().await;
        *active = Some(ActiveOAuthCallbackListener {
            flow_id,
            owner,
            provider,
            stop_tx,
            task,
        });
    }
} // end impl OAuthService

// ── Credential persistence ────────────────────────────────────────────────────

#[cfg(feature = "oauth")]
pub(crate) async fn persist_oauth_credentials(
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

// ── Internal helpers ──────────────────────────────────────────────────────────

#[cfg(feature = "oauth")]
fn validate_provider_configured(config: &AgentConfig, provider: &str) -> Result<(), String> {
    let registry = config.provider.plugin_registry();
    let is_configured = registry
        .config
        .providers
        .iter()
        .any(|cfg| cfg.name == provider);
    if !is_configured {
        Err(format!("Provider '{}' is not configured", provider))
    } else {
        Ok(())
    }
}

#[cfg(feature = "oauth")]
fn oauth_mode_for_provider(provider: &str) -> Option<&'static str> {
    if provider == "anthropic" {
        Some("max")
    } else {
        None
    }
}

// ── Callback listener task (free fn for 'static tokio::spawn) ─────────────────

#[cfg(feature = "oauth")]
async fn run_callback_listener_task(
    svc: OAuthService,
    identity: FlowIdentity,
    auto_complete_notifier: Option<AutoCompleteNotifier>,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
) {
    use axum::{
        Router,
        extract::{Query, State},
        response::Html,
        routing::get,
    };
    use std::time::Duration;

    let FlowIdentity {
        flow_id,
        owner,
        provider,
    } = identity;

    let (state_param, verifier) = {
        let map = svc.flows.lock().await;
        let Some(flow) = map.get(&flow_id) else {
            return;
        };
        if flow.owner != owner || flow.provider != provider {
            return;
        }
        (flow.state.clone(), flow.verifier.clone())
    };

    #[derive(Clone)]
    struct CbHttpState {
        expected_state: String,
        result_tx: CallbackResultSender,
    }

    #[derive(serde::Deserialize)]
    struct CbQuery {
        code: Option<String>,
        state: Option<String>,
        error: Option<String>,
    }

    async fn cb_handler(
        Query(params): Query<CbQuery>,
        State(st): State<CbHttpState>,
    ) -> Html<String> {
        let result = if let Some(error) = params.error {
            Err(format!("OAuth error: {}", error))
        } else if let Some(code) = params.code {
            let received_state = params.state.unwrap_or_default();
            if received_state != st.expected_state {
                Err("State mismatch - possible CSRF attack".to_string())
            } else {
                Ok((code, received_state))
            }
        } else {
            Err("No authorization code received".to_string())
        };

        let success = result.is_ok();
        {
            let mut tx = st.result_tx.lock().await;
            if let Some(tx) = tx.take() {
                let _ = tx.send(result);
            }
        }

        if success {
            Html("<html><head><title>Authorization Successful</title></head><body><h1>Authorization Successful</h1><p>You can close this window.</p></body></html>".to_string())
        } else {
            Html("<html><head><title>Authorization Failed</title></head><body><h1>Authorization Failed</h1><p>You can close this window.</p></body></html>".to_string())
        }
    }

    let (result_tx, callback_rx) = tokio::sync::oneshot::channel();
    let http_state = CbHttpState {
        expected_state: state_param,
        result_tx: Arc::new(Mutex::new(Some(result_tx))),
    };

    let app = Router::new()
        .route("/auth/callback", get(cb_handler))
        .route("/callback", get(cb_handler))
        .with_state(http_state);

    let tcp_listener = match tokio::net::TcpListener::bind(OAUTH_CALLBACK_BIND_ADDR).await {
        Ok(l) => l,
        Err(err) => {
            log::debug!(
                "OAuth callback listener for flow '{}' failed to bind {}: {}",
                flow_id,
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

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(tcp_listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    enum WaitResult {
        Stopped,
        Timeout,
        Callback(Result<(String, String), String>),
    }

    let wait = tokio::select! {
        _ = &mut stop_rx => WaitResult::Stopped,
        result = tokio::time::timeout(Duration::from_secs(OAUTH_CALLBACK_TIMEOUT_SECS), callback_rx) => {
            match result {
                Ok(Ok(cb)) => WaitResult::Callback(cb),
                Ok(Err(_)) => WaitResult::Callback(Err("Callback listener closed unexpectedly".to_string())),
                Err(_) => WaitResult::Timeout,
            }
        }
    };

    let _ = shutdown_tx.send(());
    let _ = server_task.await;

    let (code, cb_state) = match wait {
        WaitResult::Stopped => {
            log::debug!(
                "OAuth callback listener stopped for flow '{}' (provider='{}')",
                flow_id,
                provider
            );
            return;
        }
        WaitResult::Timeout => {
            log::debug!(
                "OAuth callback listener timed out for flow '{}' (provider='{}')",
                flow_id,
                provider
            );
            return;
        }
        WaitResult::Callback(Ok(pair)) => pair,
        WaitResult::Callback(Err(err)) => {
            log::debug!("OAuth callback error for flow '{}': {}", flow_id, err);
            return;
        }
    };

    // Verify the flow is still active.
    let flow_active = {
        let map = svc.flows.lock().await;
        map.get(&flow_id)
            .is_some_and(|f| f.owner == owner && f.provider == provider)
    };
    if !flow_active {
        return;
    }

    // Exchange tokens and persist.
    let mode = oauth_mode_for_provider(&provider);
    let oauth_provider = match crate::auth::get_oauth_provider(&provider, mode) {
        Ok(p) => p,
        Err(err) => {
            log::warn!("OAuth provider lookup failed for '{}': {}", provider, err);
            return;
        }
    };

    let result = async {
        let tokens = oauth_provider
            .exchange_code(&code, &cb_state, &verifier)
            .await
            .map_err(|e| format!("Token exchange failed: {}", e))?;
        persist_oauth_credentials(oauth_provider.as_ref(), &tokens, None).await?;
        Ok::<(), String>(())
    }
    .await;

    match result {
        Ok(()) => {
            let should_finalize = {
                let mut map = svc.flows.lock().await;
                map.remove(&flow_id).is_some()
            };
            if !should_finalize {
                return;
            }

            // Clear the listener slot since we're done.
            {
                let mut active = svc.listener_slot.lock().await;
                if active.as_ref().is_some_and(|l| l.flow_id == flow_id) {
                    active.take();
                }
            }

            svc.config.invalidate_provider_cache().await;
            svc.model_registry.invalidate_all().await;

            let completion = CompleteFlowResult {
                provider: provider.clone(),
                success: true,
                message: format!(
                    "Successfully authenticated with {}",
                    oauth_provider.display_name()
                ),
            };

            if let Some(notifier) = auto_complete_notifier.as_ref() {
                notifier(completion);
            }

            log::info!(
                "OAuth callback flow completed successfully for provider '{}' (flow '{}')",
                provider,
                flow_id
            );
        }
        Err(err) => {
            let completion = CompleteFlowResult {
                provider: provider.clone(),
                success: false,
                message: err.clone(),
            };
            if let Some(notifier) = auto_complete_notifier.as_ref() {
                notifier(completion);
            }

            log::warn!(
                "OAuth callback token exchange failed for '{}': {}",
                provider,
                err
            );
        }
    }
}

// ── Listener stop helpers ─────────────────────────────────────────────────────

#[cfg(feature = "oauth")]
async fn stop_listener(
    listener_slot: &CallbackListenerSlot,
    flows: &OAuthFlowMap,
    remove_flow: bool,
) {
    let active = {
        let mut slot = listener_slot.lock().await;
        slot.take()
    };
    if let Some(active) = active {
        if remove_flow {
            let mut map = flows.lock().await;
            map.remove(&active.flow_id);
        }
        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}

#[cfg(feature = "oauth")]
async fn stop_listener_for_flow(
    listener_slot: &CallbackListenerSlot,
    flows: &OAuthFlowMap,
    flow_id: &str,
    remove_flow: bool,
) {
    let active = {
        let mut slot = listener_slot.lock().await;
        if slot.as_ref().is_some_and(|l| l.flow_id == flow_id) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(active) = active {
        if remove_flow {
            let mut map = flows.lock().await;
            map.remove(&active.flow_id);
        }
        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}

async fn stop_listener_for_owner(listener_slot: &CallbackListenerSlot, owner: &str) {
    let active = {
        let mut slot = listener_slot.lock().await;
        if slot.as_ref().is_some_and(|l| l.owner == owner) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(active) = active {
        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}

#[cfg(feature = "oauth")]
async fn stop_listener_for_owner_provider(
    listener_slot: &CallbackListenerSlot,
    owner: &str,
    provider: &str,
) {
    let active = {
        let mut slot = listener_slot.lock().await;
        if slot
            .as_ref()
            .is_some_and(|l| l.owner == owner && l.provider == provider)
        {
            slot.take()
        } else {
            None
        }
    };
    if let Some(active) = active {
        let _ = active.stop_tx.send(());
        let _ = active.task.await;
    }
}
