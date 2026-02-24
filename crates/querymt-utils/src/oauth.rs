//! OAuth authentication and token management
//!
//! This module provides presentation-agnostic OAuth authentication flows through the
//! `OAuthUI` trait abstraction. It supports multiple OAuth providers (Anthropic, Codex)
//! with automatic token refresh and secure keyring storage.
//!
//! # Architecture
//!
//! - **`OAuthUI` trait**: Abstraction for presenting OAuth flows to users (console, web, etc.)
//! - **`OAuthProvider` trait**: Provider-specific OAuth implementations
//! - **Core functions**: `authenticate`, `refresh_tokens`, `get_valid_token`, etc.
//!
//! # Examples
//!
//! ```rust,no_run
//! use querymt_utils::oauth::{get_or_refresh_token, get_oauth_provider};
//!
//! # async fn example() -> anyhow::Result<()> {
//! // Simple token retrieval with automatic refresh
//! let token = get_or_refresh_token("anthropic").await?;
//! println!("Access token: {}", token);
//! # Ok(())
//! # }
//! ```

use crate::secret_store::SecretStore;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::time::Duration;

// Re-export types that are part of the public API
pub use anthropic_auth::{
    AsyncOAuthClient as AnthropicOAuthClient, OAuthConfig, OAuthMode, TokenSet,
};

/// Presentation-agnostic OAuth UI abstraction
///
/// Implementations of this trait define how OAuth flows are presented to users.
/// This could be a console UI (with colored output and browser opening), a web UI
/// (with QR codes or redirects), or any other presentation layer.
#[async_trait]
pub trait OAuthUI: Send + Sync {
    /// Present the authorization URL to the user and return the authorization code.
    ///
    /// The implementation decides HOW — could open a browser + run callback server,
    /// show a QR code, send a WebSocket message to a frontend, etc.
    ///
    /// # Arguments
    ///
    /// * `provider_name` - The name of the provider (e.g., "anthropic", "codex")
    /// * `url` - The authorization URL to present
    /// * `state` - The OAuth state parameter for validation
    ///
    /// # Returns
    ///
    /// The authorization code received from the user
    async fn authorize(&self, provider_name: &str, url: &str, state: &str) -> Result<String>;

    /// Optional: Handle the full OAuth exchange flow directly (e.g., via callback server).
    ///
    /// Returns `Some((tokens, optional_api_key))` if the UI handled the full exchange,
    /// or `None` to fall back to `authorize()` + `provider.exchange_code()`.
    ///
    /// This is useful for providers like Codex where a callback server can handle
    /// both code receipt and token exchange in one step.
    async fn authorize_and_exchange(
        &self,
        _provider: &dyn OAuthProvider,
        _flow: &OAuthFlowData,
    ) -> Result<Option<(TokenSet, Option<String>)>> {
        Ok(None) // Default: fall back to authorize() + exchange_code()
    }

    /// Report a status/progress message to the user
    fn status(&self, message: &str);

    /// Report a success message to the user
    fn success(&self, message: &str);

    /// Report an error/warning message to the user
    fn error(&self, message: &str);
}

/// OAuth provider abstraction
///
/// Implementations define provider-specific OAuth flows, token exchange,
/// and token refresh logic.
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Get the provider name (e.g., "anthropic", "codex")
    fn name(&self) -> &str;

    /// Get the display name for user-facing messages
    fn display_name(&self) -> &str;

    /// Start the OAuth flow and return authorization URL, state, and PKCE verifier
    async fn start_flow(&self) -> Result<OAuthFlowData>;

    /// Exchange authorization code for tokens
    async fn exchange_code(&self, code: &str, state: &str, verifier: &str) -> Result<TokenSet>;

    /// Refresh an expired token
    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet>;

    /// Optionally create an API key (for providers that support it)
    async fn create_api_key(&self, _access_token: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Get the API key environment variable name (e.g., "ANTHROPIC_API_KEY")
    fn api_key_name(&self) -> Option<&str> {
        None
    }
}

/// OAuth flow data returned when starting a flow
pub struct OAuthFlowData {
    pub authorization_url: String,
    pub state: String,
    pub verifier: String,
}

/// Anthropic OAuth provider implementation
pub struct AnthropicProvider {
    mode: OAuthMode,
}

impl AnthropicProvider {
    pub fn new(mode: OAuthMode) -> Self {
        Self { mode }
    }
}

#[async_trait]
impl OAuthProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn display_name(&self) -> &str {
        "Anthropic"
    }

    async fn start_flow(&self) -> Result<OAuthFlowData> {
        let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
        let flow = client.start_flow(self.mode)?;

        Ok(OAuthFlowData {
            authorization_url: flow.authorization_url,
            state: flow.state,
            verifier: flow.verifier,
        })
    }

    async fn exchange_code(&self, code: &str, state: &str, verifier: &str) -> Result<TokenSet> {
        let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
        Ok(client.exchange_code(code, state, verifier).await?)
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
        Ok(client.refresh_token(refresh_token).await?)
    }

    async fn create_api_key(&self, access_token: &str) -> Result<Option<String>> {
        if matches!(self.mode, OAuthMode::Console) {
            let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
            Ok(Some(client.create_api_key(access_token).await?))
        } else {
            Ok(None)
        }
    }

    fn api_key_name(&self) -> Option<&str> {
        Some("ANTHROPIC_API_KEY")
    }
}

/// Codex OAuth provider implementation
///
/// This is intentionally separate from OpenAI so tokens are stored and looked up
/// under `oauth_codex` in the keychain (no aliasing).
pub struct CodexProvider {
    client: openai_auth::OAuthClient,
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexProvider {
    pub fn new() -> Self {
        let config = openai_auth::OAuthConfig::builder()
            .redirect_port(1455)
            .build();
        let client =
            openai_auth::OAuthClient::new(config).expect("Failed to create OpenAI OAuth client");
        Self { client }
    }
}

#[async_trait]
impl OAuthProvider for CodexProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn display_name(&self) -> &str {
        "Codex"
    }

    async fn start_flow(&self) -> Result<OAuthFlowData> {
        let flow = self.client.start_flow()?;

        Ok(OAuthFlowData {
            authorization_url: flow.authorization_url,
            state: flow.state,
            verifier: flow.pkce_verifier,
        })
    }

    async fn exchange_code(&self, code: &str, _state: &str, verifier: &str) -> Result<TokenSet> {
        // Codex backend uses the OAuth access token directly.
        let tokens = self.client.exchange_code(code, verifier).await?;

        Ok(TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let tokens = self.client.refresh_token(refresh_token).await?;

        Ok(TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        })
    }
}

/// Get the appropriate OAuth provider for a given provider name
///
/// # Arguments
///
/// * `provider_name` - The name of the provider (e.g., "anthropic", "codex")
/// * `mode` - Optional mode string for providers that support multiple modes (e.g., "max", "console" for Anthropic)
///
/// # Returns
///
/// A boxed OAuth provider instance
///
/// # Errors
///
/// Returns an error if the provider doesn't support OAuth or if an invalid mode is specified
pub fn get_oauth_provider(
    provider_name: &str,
    mode: Option<&str>,
) -> Result<Box<dyn OAuthProvider>> {
    match provider_name {
        "anthropic" => {
            let oauth_mode = match mode {
                Some("max") | None => OAuthMode::Max,
                Some("console") => OAuthMode::Console,
                Some(m) => {
                    return Err(anyhow!(
                        "Invalid mode '{}' for Anthropic. Use 'max' or 'console'",
                        m
                    ));
                }
            };
            Ok(Box::new(AnthropicProvider::new(oauth_mode)))
        }
        "codex" => Ok(Box::new(CodexProvider::new())),
        _ => Err(anyhow!(
            "OAuth is not supported for provider '{}'",
            provider_name
        )),
    }
}

/// Full OAuth authentication flow
///
/// This function orchestrates the complete OAuth login process:
/// 1. Start the OAuth flow with the provider
/// 2. Present the authorization URL to the user (via UI)
/// 3. Exchange the authorization code for tokens
/// 4. Store tokens and optional API key in the secret store
///
/// # Arguments
///
/// * `provider` - The OAuth provider to authenticate with
/// * `store` - The secret store to save tokens to
/// * `ui` - The UI implementation for presenting the flow to the user
///
/// # Returns
///
/// Success or an error
pub async fn authenticate(
    provider: &dyn OAuthProvider,
    store: &mut SecretStore,
    ui: &dyn OAuthUI,
) -> Result<()> {
    ui.status(&format!(
        "=== {} OAuth Authentication ===\n",
        provider.display_name()
    ));

    ui.status(&format!(
        "Starting OAuth flow for {}...",
        provider.display_name()
    ));
    let flow = provider.start_flow().await?;

    // Try the full exchange path first (e.g., callback server)
    let (tokens, api_key) = if let Some(result) = ui.authorize_and_exchange(provider, &flow).await?
    {
        result
    } else {
        // Fall back to code-based flow
        let code = ui
            .authorize(provider.name(), &flow.authorization_url, &flow.state)
            .await?;
        ui.status("Exchanging code for tokens...");
        let tokens = provider
            .exchange_code(&code, &flow.state, &flow.verifier)
            .await?;
        let api_key = provider
            .create_api_key(&tokens.access_token)
            .await
            .ok()
            .flatten();
        (tokens, api_key)
    };

    // Store tokens
    store.set_oauth_tokens(provider.name(), &tokens)?;
    ui.success("Successfully authenticated!");

    // Store API key if we got one
    if let Some(api_key) = api_key {
        ui.status("Creating API key...");

        if let Some(key_name) = provider.api_key_name() {
            store.set(key_name, &api_key)?;
        }

        ui.success("API key created and stored!");
        ui.status(&format!(
            "Your API key has been securely stored in your system keychain.\nYou can now use it with: qmt -p {} \"your prompt\"",
            provider.name()
        ));
    } else {
        ui.status(&format!(
            "Your OAuth tokens have been securely stored in your system keychain.\nYou can now use {} with: qmt -p {} \"your prompt\"",
            provider.display_name(),
            provider.name()
        ));
    }

    Ok(())
}

/// Refresh OAuth tokens for a provider
///
/// # Arguments
///
/// * `provider` - The OAuth provider
/// * `store` - The secret store to load/save tokens
///
/// # Returns
///
/// The new token set or an error
pub async fn refresh_tokens(
    provider: &dyn OAuthProvider,
    store: &mut SecretStore,
) -> Result<TokenSet> {
    let tokens = store
        .get_oauth_tokens(provider.name())
        .ok_or_else(|| anyhow!("No OAuth tokens found for {}", provider.display_name()))?;

    let new_tokens = provider.refresh_token(&tokens.refresh_token).await?;

    // Store the new tokens
    store.set_oauth_tokens(provider.name(), &new_tokens)?;

    if let Ok(Some(api_key)) = provider.create_api_key(&new_tokens.access_token).await
        && let Some(key_name) = provider.api_key_name()
    {
        let _ = store.set(key_name, &api_key);
    }

    Ok(new_tokens)
}

/// Get a valid access token for a provider, refreshing if necessary
///
/// # Arguments
///
/// * `provider` - The OAuth provider
/// * `store` - The secret store to load tokens from
///
/// # Returns
///
/// The access token or an error
pub async fn get_valid_token(
    provider: &dyn OAuthProvider,
    store: &mut SecretStore,
) -> Result<String> {
    // Try to get valid token
    if let Some(token) = store.get_valid_access_token(provider.name()) {
        return Ok(token);
    }

    // Token is expired or missing, try to refresh
    log::info!(
        "{} OAuth token expired, attempting to refresh...",
        provider.display_name()
    );
    let new_tokens = refresh_tokens(provider, store).await?;

    Ok(new_tokens.access_token)
}

/// Display OAuth authentication status
///
/// # Arguments
///
/// * `store` - The secret store to check
/// * `provider_name` - Optional provider name to check (defaults to all supported providers)
/// * `try_refresh` - Whether to attempt automatic token refresh for expired tokens
/// * `ui` - The UI implementation for displaying status
///
/// # Returns
///
/// Success or an error
pub async fn show_auth_status(
    store: &mut SecretStore,
    provider_name: Option<&str>,
    try_refresh: bool,
    ui: &dyn OAuthUI,
) -> Result<()> {
    let providers_to_check = if let Some(p) = provider_name {
        get_oauth_provider(p, None)
            .map_err(|_| anyhow!("OAuth is not supported for provider '{}'", p))?;
        vec![p.to_string()]
    } else {
        // List all known OAuth providers
        vec!["anthropic".to_string(), "codex".to_string()]
    };

    ui.status("OAuth Authentication Status");
    ui.status("===========================\n");

    for p in providers_to_check {
        let status_msg = format!("{}: ", p);

        if let Some(tokens) = store.get_oauth_tokens(&p) {
            if tokens.is_expired() {
                // Try to refresh if enabled
                if try_refresh {
                    // Attempt to get OAuth provider and refresh
                    match get_oauth_provider(&p, None) {
                        Ok(oauth_provider) => {
                            match refresh_tokens(oauth_provider.as_ref(), store).await {
                                Ok(new_tokens) => {
                                    // Refresh successful
                                    ui.success(&format!("{}Valid ✓", status_msg));
                                    let expires_str = crate::secret_store::format_timestamp(
                                        new_tokens.expires_at,
                                    );
                                    ui.status(&format!("  Access token expires: {}", expires_str));
                                    ui.status("  Refresh token available");
                                }
                                Err(e) => {
                                    // Refresh failed
                                    ui.error(&format!("{}Expired ⚠️", status_msg));
                                    ui.status(&format!("  Token refresh failed: {}", e));
                                    ui.status(&format!(
                                        "  Run 'qmt auth login {}' to re-authenticate",
                                        p
                                    ));
                                }
                            }
                        }
                        Err(_) => {
                            // Provider doesn't support OAuth (shouldn't happen but handle it)
                            ui.error(&format!("{}Expired ⚠️", status_msg));
                            ui.status(&format!("  Run 'qmt auth login {}' to re-authenticate", p));
                        }
                    }
                } else {
                    // No refresh, just show expired status
                    ui.error(&format!("{}Expired ⚠️", status_msg));
                    ui.status(&format!("  Run 'qmt auth login {}' to re-authenticate", p));
                }
            } else {
                ui.success(&format!("{}Valid ✓", status_msg));

                let expires_str = crate::secret_store::format_timestamp(tokens.expires_at);
                ui.status(&format!("  Access token expires: {}", expires_str));
                ui.status("  Refresh token available");
            }
        } else {
            ui.status(&format!("{}Not authenticated", status_msg));
            ui.status(&format!("  Run 'qmt auth login {}' to authenticate", p));
        }

        ui.status("");
    }

    Ok(())
}

/// Run an OpenAI-compatible OAuth callback server on localhost
///
/// This is a helper function for UI implementations that want to use a callback server
/// for automatic code capture and token exchange (OpenAI-style).
///
/// # Arguments
///
/// * `port` - The port to listen on
/// * `state` - The OAuth state parameter for validation
/// * `verifier` - The PKCE verifier
/// * `timeout` - How long to wait for the callback
///
/// # Returns
///
/// A tuple of (TokenSet, optional API key) or an error
pub async fn openai_callback_server(
    port: u16,
    state: &str,
    verifier: &str,
    timeout: Duration,
) -> Result<(TokenSet, Option<String>)> {
    use openai_auth::run_callback_server;

    // Create a temporary client for the callback server
    let config = openai_auth::OAuthConfig::builder()
        .redirect_port(port)
        .build();
    let client = openai_auth::OAuthClient::new(config)?;

    // Start callback server with timeout
    let tokens_future = run_callback_server(port, state, &client, verifier);

    match tokio::time::timeout(timeout, tokens_future).await {
        Ok(Ok(tokens)) => {
            // Extract API key if present
            let api_key = tokens.api_key.clone();

            // Convert openai_auth::TokenSet to anthropic_auth::TokenSet
            let token_set = TokenSet {
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
                expires_at: tokens.expires_at,
            };

            Ok((token_set, api_key))
        }
        Ok(Err(e)) => Err(anyhow!("Callback server error: {}", e)),
        Err(_) => Err(anyhow!("Timeout waiting for OAuth callback")),
    }
}

/// Run an Anthropic OAuth callback server on localhost.
///
/// This helper waits for the callback, then exchanges the received authorization
/// code for OAuth tokens. In console mode, it also attempts API key creation.
///
/// # Arguments
///
/// * `port` - The port to listen on
/// * `state` - The OAuth state parameter for validation
/// * `verifier` - The PKCE verifier
/// * `mode` - Anthropic OAuth mode (`max` or `console`)
/// * `timeout` - How long to wait for the callback
///
/// # Returns
///
/// A tuple of (TokenSet, optional API key) or an error
pub async fn anthropic_callback_server(
    port: u16,
    state: &str,
    verifier: &str,
    mode: OAuthMode,
    timeout: Duration,
) -> Result<(TokenSet, Option<String>)> {
    use anthropic_auth::run_callback_server;

    // Start callback server with timeout
    let callback_future = run_callback_server(port, state);

    let callback = match tokio::time::timeout(timeout, callback_future).await {
        Ok(Ok(callback)) => callback,
        Ok(Err(e)) => return Err(anyhow!("Callback server error: {}", e)),
        Err(_) => return Err(anyhow!("Timeout waiting for OAuth callback")),
    };

    let client = AnthropicOAuthClient::new(OAuthConfig::default())?;
    let code_with_state = format!("{}#{}", callback.code, callback.state);
    let tokens = client
        .exchange_code(&code_with_state, state, verifier)
        .await?;

    let api_key = if matches!(mode, OAuthMode::Console) {
        Some(client.create_api_key(&tokens.access_token).await?)
    } else {
        None
    };

    Ok((tokens, api_key))
}

/// Extract authorization code from query string or URL
///
/// Handles both full callback URLs and query strings.
///
/// # Arguments
///
/// * `input` - The URL or query string to parse
///
/// # Returns
///
/// The extracted code, if found
pub fn extract_code_from_query(input: &str) -> Option<String> {
    use url::Url;

    // Handle full URLs like http://localhost:1455/auth/callback?code=xxx&state=yyy
    if input.starts_with("http") {
        if let Ok(url) = Url::parse(input) {
            for (key, value) in url.query_pairs() {
                if key == "code" {
                    return Some(value.into_owned());
                }
            }
        }
        return None;
    }

    // Handle query string like ?code=xxx&state=yyy or code=xxx&state=yyy
    let query = input.trim_start_matches('?');
    for part in query.split('&') {
        if let Some((key, value)) = part.split_once('=')
            && key == "code"
        {
            return Some(value.to_string());
        }
    }

    None
}

/// Get a valid OAuth token for a provider, refreshing if necessary
///
/// This is a simplified convenience function that doesn't require passing a SecretStore.
/// It's kept for backward compatibility with existing agent code.
///
/// # Arguments
///
/// * `provider` - The provider name (e.g., "anthropic", "codex")
///
/// # Returns
///
/// The access token or an error
///
/// # Examples
///
/// ```rust,no_run
/// use querymt_utils::oauth::get_or_refresh_token;
///
/// # async fn example() -> anyhow::Result<()> {
/// let token = get_or_refresh_token("anthropic").await?;
/// println!("Got access token: {}", token);
/// # Ok(())
/// # }
/// ```
pub async fn get_or_refresh_token(provider: &str) -> Result<String> {
    log::debug!("Checking OAuth tokens for provider: {}", provider);

    let mut store = SecretStore::new().map_err(|e| anyhow!("Keyring access failed: {}", e))?;

    let oauth_provider = get_oauth_provider(provider, None)?;
    get_valid_token(oauth_provider.as_ref(), &mut store).await
}
