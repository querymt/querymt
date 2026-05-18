//! OAuth authentication and token management
//!
//! This module provides presentation-agnostic OAuth authentication flows through the
//! `OAuthUI` trait abstraction. It supports multiple OAuth providers (Anthropic, Codex, Kimi Code)
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use crate::OAuthFlowKind;

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
    /// * `provider_name` - The name of the provider (e.g., "anthropic", "codex", "kimi-code")
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
    /// Get the provider name (e.g., "anthropic", "codex", "kimi-code")
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
    async fn create_api_key(&self, access_token: &str) -> Result<Option<String>>;

    /// Get the API key environment variable name (e.g., "ANTHROPIC_API_KEY")
    fn api_key_name(&self) -> Option<&str>;

    /// The OAuth flow interaction mode for this provider.
    fn flow_kind(&self) -> OAuthFlowKind {
        OAuthFlowKind::RedirectCode
    }

    /// Local loopback callback port for redirect-code flows, if supported.
    fn callback_port(&self) -> Option<u16> {
        if self.flow_kind() == OAuthFlowKind::RedirectCode {
            Some(1455)
        } else {
            None
        }
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

fn convert_openai_tokens(tokens: openai_auth::TokenSet) -> TokenSet {
    TokenSet {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
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

        Ok(convert_openai_tokens(tokens))
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let tokens = self.client.refresh_token(refresh_token).await?;

        Ok(convert_openai_tokens(tokens))
    }

    async fn create_api_key(&self, _access_token: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn api_key_name(&self) -> Option<&str> {
        None
    }
}

/// xAI Grok OAuth provider implementation.
///
/// Uses xAI's OIDC discovery and PKCE requirements directly. The default client
/// ID is the upstream Grok CLI client ID observed in Hermes; set
/// `XAI_OAUTH_CLIENT_ID` to override it once xAI provides a QueryMT client.
pub struct XaiProvider;

#[derive(Debug, Clone)]
struct XaiOidcEndpoints {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, serde::Deserialize)]
struct XaiDiscoveryDocument {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct XaiFlowSnapshot {
    code_verifier: String,
    code_challenge: String,
    token_endpoint: String,
    redirect_uri: String,
    client_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct XaiTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    expires_at: Option<u64>,
}

impl Default for XaiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl XaiProvider {
    const DEFAULT_CLIENT_ID: &'static str = "b1a00492-073a-47ea-816f-4c329264a828";
    const DEFAULT_DISCOVERY_URL: &'static str =
        "https://auth.x.ai/.well-known/openid-configuration";
    const DEFAULT_REDIRECT_URI: &'static str = "http://127.0.0.1:56121/callback";
    const SCOPE: &'static str = "openid profile email offline_access grok-cli:access api:access";

    pub fn new() -> Self {
        Self
    }

    async fn discover_endpoints() -> Result<XaiOidcEndpoints> {
        let auth_url = std::env::var("XAI_OAUTH_AUTH_URL").ok();
        let token_url = std::env::var("XAI_OAUTH_TOKEN_URL").ok();
        if let (Some(authorization_endpoint), Some(token_endpoint)) =
            (auth_url.clone(), token_url.clone())
        {
            return Ok(XaiOidcEndpoints {
                authorization_endpoint,
                token_endpoint,
            });
        }

        let discovery: XaiDiscoveryDocument = reqwest::Client::new()
            .get(Self::DEFAULT_DISCOVERY_URL)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Self::validate_discovered_endpoint(&discovery.authorization_endpoint)?;
        Self::validate_discovered_endpoint(&discovery.token_endpoint)?;

        Ok(XaiOidcEndpoints {
            authorization_endpoint: auth_url.unwrap_or(discovery.authorization_endpoint),
            token_endpoint: token_url.unwrap_or(discovery.token_endpoint),
        })
    }

    async fn token_endpoint() -> Result<String> {
        if let Ok(token_url) = std::env::var("XAI_OAUTH_TOKEN_URL") {
            return Ok(token_url);
        }
        Ok(Self::discover_endpoints().await?.token_endpoint)
    }

    fn client_id() -> String {
        std::env::var("XAI_OAUTH_CLIENT_ID").unwrap_or_else(|_| Self::DEFAULT_CLIENT_ID.to_string())
    }

    fn redirect_uri() -> String {
        std::env::var("XAI_OAUTH_REDIRECT_URI")
            .unwrap_or_else(|_| Self::DEFAULT_REDIRECT_URI.to_string())
    }

    fn validate_discovered_endpoint(endpoint: &str) -> Result<()> {
        let url = url::Url::parse(endpoint)?;
        let host = url.host_str().unwrap_or_default();
        if url.scheme() != "https" || host != "auth.x.ai" {
            return Err(anyhow!(
                "xAI discovery returned unexpected endpoint: {}",
                endpoint
            ));
        }
        Ok(())
    }

    fn generate_random_urlsafe(byte_len: usize) -> Result<String> {
        use base64::Engine as _;

        let mut bytes = vec![0u8; byte_len];
        getrandom::getrandom(&mut bytes)?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    fn code_challenge(verifier: &str) -> String {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        let digest = Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    }

    fn build_authorization_url(
        authorization_endpoint: &str,
        client_id: &str,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        code_challenge: &str,
    ) -> Result<String> {
        let mut url = url::Url::parse(authorization_endpoint)?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", Self::SCOPE)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("plan", "generic")
            .append_pair("referrer", "querymt");
        Ok(url.into())
    }

    fn exchange_form<'a>(code: &'a str, snapshot: &'a XaiFlowSnapshot) -> Vec<(&'a str, &'a str)> {
        vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", snapshot.redirect_uri.as_str()),
            ("client_id", snapshot.client_id.as_str()),
            ("code_verifier", snapshot.code_verifier.as_str()),
            ("code_challenge", snapshot.code_challenge.as_str()),
            ("code_challenge_method", "S256"),
        ]
    }

    fn refresh_form<'a>(client_id: &'a str, refresh_token: &'a str) -> Vec<(&'a str, &'a str)> {
        vec![
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
        ]
    }

    fn encode_form(form: &[(&str, &str)]) -> String {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in form {
            serializer.append_pair(key, value);
        }
        serializer.finish()
    }

    fn token_set(response: XaiTokenResponse, old_refresh_token: Option<&str>) -> Result<TokenSet> {
        let access_token = response
            .access_token
            .ok_or_else(|| anyhow!("xAI token response did not include access_token"))?;
        let refresh_token = response
            .refresh_token
            .or_else(|| old_refresh_token.map(str::to_string))
            .ok_or_else(|| anyhow!("xAI token response did not include refresh_token"))?;
        let expires_at = response.expires_at.or_else(|| {
            response
                .expires_in
                .map(|expires_in| current_epoch_seconds() + expires_in.saturating_sub(120))
        });

        Ok(TokenSet {
            access_token,
            refresh_token,
            expires_at: expires_at.unwrap_or(0),
        })
    }
}

fn current_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[async_trait]
impl OAuthProvider for XaiProvider {
    fn name(&self) -> &str {
        "xai"
    }

    fn display_name(&self) -> &str {
        "xAI Grok"
    }

    async fn start_flow(&self) -> Result<OAuthFlowData> {
        let endpoints = Self::discover_endpoints().await?;
        let client_id = Self::client_id();
        let redirect_uri = Self::redirect_uri();
        let state = Self::generate_random_urlsafe(32)?;
        let nonce = Self::generate_random_urlsafe(32)?;
        let code_verifier = Self::generate_random_urlsafe(32)?;
        let code_challenge = Self::code_challenge(&code_verifier);
        let authorization_url = Self::build_authorization_url(
            &endpoints.authorization_endpoint,
            &client_id,
            &redirect_uri,
            &state,
            &nonce,
            &code_challenge,
        )?;
        let verifier = serde_json::to_string(&XaiFlowSnapshot {
            code_verifier,
            code_challenge,
            token_endpoint: endpoints.token_endpoint,
            redirect_uri,
            client_id,
        })?;

        Ok(OAuthFlowData {
            authorization_url,
            state,
            verifier,
        })
    }

    async fn exchange_code(&self, code: &str, _state: &str, verifier: &str) -> Result<TokenSet> {
        let snapshot: XaiFlowSnapshot = serde_json::from_str(verifier)
            .map_err(|e| anyhow!("Invalid xAI OAuth flow data: {}", e))?;
        let body = Self::encode_form(&Self::exchange_form(code, &snapshot));
        let response: XaiTokenResponse = reqwest::Client::new()
            .post(&snapshot.token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Self::token_set(response, None)
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let token_endpoint = Self::token_endpoint().await?;
        let client_id = Self::client_id();
        let body = Self::encode_form(&Self::refresh_form(&client_id, refresh_token));
        let response: XaiTokenResponse = reqwest::Client::new()
            .post(&token_endpoint)
            .header(reqwest::header::ACCEPT, "application/json")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Self::token_set(response, Some(refresh_token))
    }

    async fn create_api_key(&self, _access_token: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn api_key_name(&self) -> Option<&str> {
        Some("XAI_API_KEY")
    }

    fn callback_port(&self) -> Option<u16> {
        Some(56121)
    }
}

/// Kimi Code OAuth provider implementation.
///
/// Uses Kimi's OAuth device flow and stores tokens under `oauth_kimi-code` in keychain.
pub struct KimiCodeProvider;

impl Default for KimiCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl KimiCodeProvider {
    pub fn new() -> Self {
        Self
    }

    fn convert_tokens(tokens: kimi_auth::TokenSet) -> TokenSet {
        TokenSet {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: tokens.expires_at,
        }
    }

    fn oauth_config() -> kimi_auth::OAuthConfig {
        kimi_auth::kimi_cli_oauth_config()
    }
}

#[async_trait]
impl OAuthProvider for KimiCodeProvider {
    fn name(&self) -> &str {
        "kimi-code"
    }

    fn display_name(&self) -> &str {
        "Kimi Code"
    }

    async fn start_flow(&self) -> Result<OAuthFlowData> {
        let config = Self::oauth_config();
        let client = kimi_auth::AsyncOAuthClient::new(config.clone())?;
        let flow = client.start_flow().await?;

        let snapshot = kimi_auth::OAuthFlowState::new(config, flow.clone());
        let verifier = serde_json::to_string(&snapshot)
            .map_err(|e| anyhow!("Failed to serialize Kimi OAuth flow data: {}", e))?;

        Ok(OAuthFlowData {
            authorization_url: flow.verification_uri_complete.clone(),
            state: flow.user_code,
            verifier,
        })
    }

    async fn exchange_code(&self, _code: &str, _state: &str, verifier: &str) -> Result<TokenSet> {
        let snapshot: kimi_auth::OAuthFlowState = serde_json::from_str(verifier)
            .map_err(|e| anyhow!("Invalid Kimi OAuth flow data: {}", e))?;
        let (config, flow) = snapshot.into_parts();

        let client = kimi_auth::AsyncOAuthClient::new(config)?;
        let tokens = client.poll_for_token(&flow).await?;
        Ok(Self::convert_tokens(tokens))
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let client = kimi_auth::AsyncOAuthClient::new(Self::oauth_config())?;
        let tokens = client.refresh_token(refresh_token).await?;
        Ok(Self::convert_tokens(tokens))
    }

    async fn create_api_key(&self, _access_token: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn api_key_name(&self) -> Option<&str> {
        None
    }

    fn flow_kind(&self) -> OAuthFlowKind {
        OAuthFlowKind::DevicePoll
    }
}

/// Get the appropriate OAuth provider for a given provider name
///
/// # Arguments
///
/// * `provider_name` - The name of the provider (e.g., "anthropic", "codex", "kimi-code")
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
        "xai" => Ok(Box::new(XaiProvider::new())),
        "kimi-code" => Ok(Box::new(KimiCodeProvider::new())),
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
        vec![
            "anthropic".to_string(),
            "codex".to_string(),
            "xai".to_string(),
            "kimi-code".to_string(),
        ]
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
/// * `provider` - The provider name (e.g., "anthropic", "codex", "kimi-code")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_flow_kinds() {
        let anthropic = AnthropicProvider::new(OAuthMode::Max);
        assert_eq!(anthropic.flow_kind(), OAuthFlowKind::RedirectCode);
        assert_eq!(anthropic.callback_port(), Some(1455));
        assert_eq!(anthropic.name(), "anthropic");

        let codex = CodexProvider::new();
        assert_eq!(codex.flow_kind(), OAuthFlowKind::RedirectCode);
        assert_eq!(codex.callback_port(), Some(1455));
        assert_eq!(codex.name(), "codex");

        let xai = XaiProvider::new();
        assert_eq!(xai.flow_kind(), OAuthFlowKind::RedirectCode);
        assert_eq!(xai.callback_port(), Some(56121));
        assert_eq!(xai.name(), "xai");

        let kimi = KimiCodeProvider::new();
        assert_eq!(kimi.flow_kind(), OAuthFlowKind::DevicePoll);
        assert_eq!(kimi.name(), "kimi-code");
    }

    #[test]
    fn get_oauth_provider_returns_correct_flow_kinds() {
        let anthropic = get_oauth_provider("anthropic", None).unwrap();
        assert_eq!(anthropic.flow_kind(), OAuthFlowKind::RedirectCode);

        let codex = get_oauth_provider("codex", None).unwrap();
        assert_eq!(codex.flow_kind(), OAuthFlowKind::RedirectCode);
        assert_eq!(codex.callback_port(), Some(1455));

        let xai = get_oauth_provider("xai", None).unwrap();
        assert_eq!(xai.flow_kind(), OAuthFlowKind::RedirectCode);
        assert_eq!(xai.callback_port(), Some(56121));

        let kimi = get_oauth_provider("kimi-code", None).unwrap();
        assert_eq!(kimi.flow_kind(), OAuthFlowKind::DevicePoll);
    }

    #[test]
    fn xai_authorization_url_uses_required_parameters() {
        let state = "state-value";
        let nonce = "nonce-value";
        let challenge = "challenge-value";
        let url = XaiProvider::build_authorization_url(
            "https://auth.x.ai/oauth/authorize",
            XaiProvider::DEFAULT_CLIENT_ID,
            XaiProvider::DEFAULT_REDIRECT_URI,
            state,
            nonce,
            challenge,
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(
            parsed.as_str().split('?').next(),
            Some("https://auth.x.ai/oauth/authorize")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some(XaiProvider::DEFAULT_CLIENT_ID)
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(XaiProvider::SCOPE)
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:56121/callback")
        );
        assert_eq!(
            params.get("code_challenge").map(String::as_str),
            Some(challenge)
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(params.get("state").map(String::as_str), Some(state));
        assert_eq!(params.get("nonce").map(String::as_str), Some(nonce));
        assert_eq!(params.get("plan").map(String::as_str), Some("generic"));
        assert_eq!(params.get("referrer").map(String::as_str), Some("querymt"));
    }

    #[test]
    fn xai_verifier_snapshot_carries_pkce_and_token_endpoint() {
        let snapshot = XaiFlowSnapshot {
            code_verifier: "verifier".to_string(),
            code_challenge: XaiProvider::code_challenge("verifier"),
            token_endpoint: "https://auth.x.ai/oauth/token".to_string(),
            redirect_uri: XaiProvider::DEFAULT_REDIRECT_URI.to_string(),
            client_id: XaiProvider::DEFAULT_CLIENT_ID.to_string(),
        };
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let decoded: XaiFlowSnapshot = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.code_verifier, "verifier");
        assert!(!decoded.code_challenge.is_empty());
        assert_eq!(decoded.token_endpoint, "https://auth.x.ai/oauth/token");
    }

    #[test]
    fn xai_exchange_form_echoes_pkce_challenge() {
        let snapshot = XaiFlowSnapshot {
            code_verifier: "verifier".to_string(),
            code_challenge: "challenge".to_string(),
            token_endpoint: "https://auth.x.ai/oauth/token".to_string(),
            redirect_uri: XaiProvider::DEFAULT_REDIRECT_URI.to_string(),
            client_id: XaiProvider::DEFAULT_CLIENT_ID.to_string(),
        };
        let form: std::collections::HashMap<_, _> =
            XaiProvider::exchange_form("auth-code", &snapshot)
                .into_iter()
                .collect();

        assert_eq!(form.get("grant_type"), Some(&"authorization_code"));
        assert_eq!(form.get("code"), Some(&"auth-code"));
        assert_eq!(form.get("client_id"), Some(&XaiProvider::DEFAULT_CLIENT_ID));
        assert_eq!(
            form.get("redirect_uri"),
            Some(&XaiProvider::DEFAULT_REDIRECT_URI)
        );
        assert_eq!(form.get("code_verifier"), Some(&"verifier"));
        assert_eq!(form.get("code_challenge"), Some(&"challenge"));
        assert_eq!(form.get("code_challenge_method"), Some(&"S256"));
    }

    #[test]
    fn xai_refresh_preserves_existing_refresh_token_when_omitted() {
        let tokens = XaiProvider::token_set(
            XaiTokenResponse {
                access_token: Some("access".to_string()),
                refresh_token: None,
                expires_in: Some(3600),
                expires_at: None,
            },
            Some("old-refresh"),
        )
        .unwrap();

        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token, "old-refresh");
        assert!(tokens.expires_at > current_epoch_seconds());
    }

    #[test]
    fn get_oauth_provider_rejects_unknown() {
        assert!(get_oauth_provider("unknown-provider", None).is_err());
    }
}
