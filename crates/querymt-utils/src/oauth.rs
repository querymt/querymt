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
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeVerifier, RedirectUrl, RefreshToken,
    Scope, TokenResponse, TokenUrl,
};
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

    fn callback_port(&self) -> Option<u16> {
        // This client uses the hosted code callback, not a loopback redirect.
        None
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
        for attempt in 0..=2 {
            match client.refresh_token(refresh_token).await {
                Ok(tokens) => return Ok(tokens),
                Err(error) => {
                    if !anthropic_refresh_retryable(&error) || attempt == 2 {
                        return Err(map_anthropic_refresh_error(self.display_name(), error));
                    }
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                }
            }
        }
        unreachable!("bounded refresh loop always returns")
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

/// Whether a failed Anthropic refresh is worth retrying immediately.
fn anthropic_refresh_retryable(error: &anthropic_auth::AnthropicAuthError) -> bool {
    error.is_transient()
}

/// OAuth error codes meaning the refresh token itself is revoked, expired or
/// already rotated, so only signing in again helps.
const REFRESH_REJECTION_CODES: &[&str] = &[
    "invalid_grant",
    "refresh_token_expired",
    "refresh_token_reused",
    "refresh_token_invalidated",
];

/// The refresh-rejection code in an HTTP error body, if there is one.
///
/// Looks for a JSON object anywhere in `body` (libraries prefix it with the
/// status line) and reads its `error` member, either a string
/// (`{"error":"invalid_grant"}`) or an object with a `code`/`type`.
/// Non-JSON bodies, such as an HTML page from a proxy, never match.
fn refresh_rejection_code(body: &str) -> Option<&'static str> {
    let json = body.get(body.find('{')?..=body.rfind('}')?)?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let error = value.get("error")?;
    let code = match error {
        serde_json::Value::String(code) => code.as_str(),
        serde_json::Value::Object(map) => map
            .get("code")
            .or_else(|| map.get("type"))
            .and_then(|code| code.as_str())?,
        _ => return None,
    };
    REFRESH_REJECTION_CODES
        .iter()
        .copied()
        .find(|known| *known == code)
}

/// Map a final Anthropic refresh failure to an error callers can classify.
///
/// Only a 400/401/403 whose body carries a refresh-rejection code such as
/// `invalid_grant` means the login is dead. Any other failure (a proxy error
/// page, an unrelated 4xx) is passed through unchanged.
fn map_anthropic_refresh_error(
    display: &str,
    error: anthropic_auth::AnthropicAuthError,
) -> anyhow::Error {
    if error.is_credential_rejected()
        && let anthropic_auth::AnthropicAuthError::Http { body, .. } = &error
        && let Some(code) = refresh_rejection_code(body)
    {
        return anyhow::Error::new(TokenRefreshError::relogin(display, code));
    }
    error.into()
}

/// Map a Codex refresh failure to an error callers can classify.
///
/// `openai-auth` reports a rejected refresh as an HTTP-style error carrying the
/// response body. Only a rejection code in that body means signing in again is
/// required; connection failures and server outages stay transient.
fn map_codex_refresh_error(display: &str, error: openai_auth::OpenAIAuthError) -> anyhow::Error {
    use openai_auth::OpenAIAuthError::{ApiKeyExchange, Http};
    if let ApiKeyExchange { status, body } | Http { status, body } = &error
        && matches!(status, 400 | 401 | 403)
        && let Some(code) = refresh_rejection_code(body)
    {
        return anyhow::Error::new(TokenRefreshError::relogin(display, code));
    }
    error.into()
}

fn convert_openai_tokens(tokens: openai_auth::TokenSet) -> TokenSet {
    TokenSet {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
    }
}

async fn timeout_callback_result<T, E, F>(timeout: Duration, future: F) -> Result<T>
where
    E: std::fmt::Display,
    F: std::future::Future<Output = std::result::Result<T, E>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(anyhow!("Callback server error: {}", err)),
        Err(_) => Err(anyhow!("Timeout waiting for OAuth callback")),
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
        let code = extract_code_from_query(code).unwrap_or_else(|| code.to_string());
        // Codex backend uses the OAuth access token directly.
        let tokens = self.client.exchange_code(&code, verifier).await?;

        Ok(convert_openai_tokens(tokens))
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let tokens = self
            .client
            .refresh_token(refresh_token)
            .await
            .map_err(|error| map_codex_refresh_error(self.display_name(), error))?;

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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct XaiFlowSnapshot {
    code_verifier: String,
    code_challenge: String,
    token_endpoint: String,
    redirect_uri: String,
    client_id: String,
}

type XaiTokenResponse = oauth2::basic::BasicTokenResponse;

/// A token refresh that failed even though OAuth tokens are stored.
///
/// Callers use this to tell "the user is not logged in" (no tokens, so another
/// credential source is fine) from "the user is logged in but the login no
/// longer works" (falling back to another credential silently hides the real
/// problem).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRefreshError {
    provider: String,
    detail: String,
    relogin_required: bool,
}

impl TokenRefreshError {
    /// Provider display name, e.g. `Anthropic`.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Short, secret-free description of what the server or transport said.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The server rejected the refresh token itself (revoked, expired or
    /// already rotated); only signing in again can fix this. When false the
    /// failure is transient and a later attempt may succeed.
    pub fn relogin_required(&self) -> bool {
        self.relogin_required
    }

    /// The same error with the remaining back-off appended to the detail.
    fn retrying_in(&self, remaining: Duration) -> Self {
        Self {
            detail: format!(
                "{}; retrying in {}s",
                self.detail,
                remaining.as_secs().max(1)
            ),
            ..self.clone()
        }
    }

    fn relogin(provider: &str, detail: impl Into<String>) -> Self {
        Self {
            provider: provider.to_string(),
            detail: detail.into(),
            relogin_required: true,
        }
    }

    fn failed(provider: &str, detail: impl Into<String>) -> Self {
        Self {
            provider: provider.to_string(),
            detail: detail.into(),
            relogin_required: false,
        }
    }
}

impl std::fmt::Display for TokenRefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.relogin_required {
            write!(
                f,
                "{} OAuth login is no longer valid ({}); sign in again",
                self.provider, self.detail
            )
        } else {
            write!(
                f,
                "{} OAuth token refresh failed: {}",
                self.provider, self.detail
            )
        }
    }
}

impl std::error::Error for TokenRefreshError {}

/// The [`TokenRefreshError`] inside `error`, if it is one.
pub fn token_refresh_error(error: &anyhow::Error) -> Option<&TokenRefreshError> {
    error.downcast_ref::<TokenRefreshError>()
}

/// Structured error type for xAI OAuth operations.
#[derive(Debug, Clone)]
pub enum XaiOAuthError {
    TokenExchangeFailed(String),
    RefreshFailed {
        message: String,
        relogin_required: bool,
    },
    StateMismatch,
    MissingRefreshToken,
    Internal(String),
}

impl std::fmt::Display for XaiOAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XaiOAuthError::TokenExchangeFailed(msg) => {
                write!(f, "xAI token exchange failed: {}", msg)
            }
            XaiOAuthError::RefreshFailed { message, .. } => {
                write!(f, "xAI token refresh failed: {}", message)
            }
            XaiOAuthError::StateMismatch => write!(f, "xAI OAuth state mismatch"),
            XaiOAuthError::MissingRefreshToken => {
                write!(f, "xAI token response did not include refresh_token")
            }
            XaiOAuthError::Internal(msg) => write!(f, "xAI OAuth error: {}", msg),
        }
    }
}

impl std::error::Error for XaiOAuthError {}

impl Default for XaiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl XaiProvider {
    const DEFAULT_CLIENT_ID: &'static str = "b1a00492-073a-47ea-816f-4c329264a828";
    const DEFAULT_AUTH_URL: &'static str = "https://auth.x.ai/oauth2/authorize";
    const DEFAULT_TOKEN_URL: &'static str = "https://auth.x.ai/oauth2/token";
    const DEFAULT_REDIRECT_URI: &'static str = "http://127.0.0.1:56121/callback";
    const SCOPE: &'static str = "openid profile email offline_access grok-cli:access api:access";

    pub fn new() -> Self {
        Self
    }

    fn generate_random_urlsafe(byte_len: usize) -> Result<String> {
        use base64::Engine as _;

        let mut bytes = vec![0u8; byte_len];
        getrandom::fill(&mut bytes)?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    fn code_challenge(verifier: &str) -> String {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        let digest = Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    }

    fn oauth_client(
        authorization_endpoint: &str,
        token_endpoint: &str,
        client_id: &str,
    ) -> Result<
        oauth2::basic::BasicClient<
            oauth2::EndpointSet,
            oauth2::EndpointNotSet,
            oauth2::EndpointNotSet,
            oauth2::EndpointNotSet,
            oauth2::EndpointSet,
        >,
    > {
        let auth_url = AuthUrl::from_url(
            authorization_endpoint
                .parse()
                .map_err(|_| anyhow!("invalid auth url"))?,
        );
        let token_url = TokenUrl::from_url(
            token_endpoint
                .parse()
                .map_err(|_| anyhow!("invalid token url"))?,
        );

        Ok(BasicClient::new(ClientId::new(client_id.to_string()))
            .set_auth_uri(auth_url)
            .set_token_uri(token_url))
    }

    fn redirect_url(redirect_uri: &str) -> Result<RedirectUrl> {
        Ok(RedirectUrl::new(redirect_uri.to_string())?)
    }

    fn build_authorization_url(
        authorization_endpoint: &str,
        client_id: &str,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        code_challenge: &str,
    ) -> Result<String> {
        let client =
            Self::oauth_client(authorization_endpoint, Self::DEFAULT_TOKEN_URL, client_id)?
                .set_redirect_uri(Self::redirect_url(redirect_uri)?);
        let (authorization_url, oauth_state) = client
            .authorize_url(|| CsrfToken::new(state.to_string()))
            .add_scope(Scope::new(Self::SCOPE.to_string()))
            .add_extra_param("code_challenge", code_challenge)
            .add_extra_param("code_challenge_method", "S256")
            .add_extra_param("nonce", nonce)
            .add_extra_param("plan", "generic")
            .add_extra_param("referrer", "querymt")
            .url();

        debug_assert_eq!(oauth_state.secret(), state);
        Ok(authorization_url.to_string())
    }

    #[cfg(test)]
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

    fn token_set(response: XaiTokenResponse, old_refresh_token: Option<&str>) -> Result<TokenSet> {
        let access_token = response.access_token().secret().to_string();
        let refresh_token = response
            .refresh_token()
            .map(|token| token.secret().to_string())
            .or_else(|| old_refresh_token.map(str::to_string))
            .ok_or_else(|| anyhow!("xAI token response did not include refresh_token"))?;
        let expires_at = response
            .expires_in()
            .map(|expires_in| current_epoch_seconds() + expires_in.as_secs().saturating_sub(120))
            .unwrap_or(0);

        Ok(TokenSet {
            access_token,
            refresh_token,
            expires_at,
        })
    }

    fn oauth_http_client() -> Result<oauth2::reqwest::Client> {
        oauth2::reqwest::ClientBuilder::new()
            .redirect(oauth2::reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| anyhow!("failed to build xAI OAuth HTTP client: {}", error))
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
        let client_id = Self::DEFAULT_CLIENT_ID.to_string();
        let redirect_uri = Self::DEFAULT_REDIRECT_URI.to_string();
        let state = Self::generate_random_urlsafe(32)?;
        let nonce = Self::generate_random_urlsafe(32)?;
        let code_verifier = PkceCodeVerifier::new(Self::generate_random_urlsafe(32)?);
        let code_challenge = Self::code_challenge(code_verifier.secret());
        let authorization_url = Self::build_authorization_url(
            Self::DEFAULT_AUTH_URL,
            &client_id,
            &redirect_uri,
            &state,
            &nonce,
            &code_challenge,
        )?;
        let verifier = serde_json::to_string(&XaiFlowSnapshot {
            code_verifier: code_verifier.secret().to_string(),
            code_challenge,
            token_endpoint: Self::DEFAULT_TOKEN_URL.to_string(),
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
        let code = extract_code_from_query(code).unwrap_or_else(|| code.to_string());
        let snapshot: XaiFlowSnapshot = serde_json::from_str(verifier)
            .map_err(|e| anyhow!("Invalid xAI OAuth flow data: {}", e))?;
        let http_client = Self::oauth_http_client()?;
        let client = Self::oauth_client(
            &snapshot.token_endpoint,
            &snapshot.token_endpoint,
            &snapshot.client_id,
        )?
        .set_redirect_uri(Self::redirect_url(&snapshot.redirect_uri)?);
        let response = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(PkceCodeVerifier::new(snapshot.code_verifier.clone()))
            .add_extra_param("code_challenge", snapshot.code_challenge.clone())
            .add_extra_param("code_challenge_method", "S256")
            .request_async(&http_client)
            .await
            .map_err(|error| anyhow!(XaiOAuthError::TokenExchangeFailed(error.to_string())))?;

        Self::token_set(response, None)
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<TokenSet> {
        let token_endpoint = Self::DEFAULT_TOKEN_URL;
        let client_id = Self::DEFAULT_CLIENT_ID.to_string();
        let http_client = Self::oauth_http_client()?;
        let client = Self::oauth_client(token_endpoint, token_endpoint, &client_id)?;
        let response = client
            .exchange_refresh_token(&RefreshToken::new(refresh_token.to_string()))
            .request_async(&http_client)
            .await
            .map_err(|error| {
                // Only an error response from the token endpoint means the
                // refresh token was rejected; transport or parse failures
                // may succeed on a later attempt.
                let relogin_required =
                    matches!(error, oauth2::RequestTokenError::ServerResponse(_));
                anyhow!(XaiOAuthError::RefreshFailed {
                    message: error.to_string(),
                    relogin_required,
                })
            })?;

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

    // Serialize with any in-flight refresh so it cannot overwrite the fresh
    // login tokens (in the cache or the keyring) after we store them.
    let token_guard = lock_provider_tokens(provider.name()).await;

    let provider_name = provider.name().to_owned();
    let key_name = provider.api_key_name().map(str::to_owned);
    let has_api_key = api_key.is_some();
    store
        .run(move |store| {
            let _guard = token_guard;
            store.set_oauth_tokens(&provider_name, &tokens)?;
            invalidate_cached_tokens(&provider_name);
            if let (Some(key_name), Some(api_key)) = (key_name, api_key) {
                store.set(key_name, api_key)?;
            }
            Ok(())
        })
        .await?;
    ui.success("Successfully authenticated!");

    if has_api_key {
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

/// Upper bound for one provider token refresh, including provider-side retries
/// and api-key creation.
///
/// The underlying OAuth HTTP clients do not configure a request timeout, so a
/// stalled connection would otherwise hold the per-provider refresh lock in
/// [`get_or_refresh_token`] indefinitely and block all subsequent token reads
/// for that provider.
const TOKEN_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `future` under [`TOKEN_REFRESH_TIMEOUT`].
async fn bound_refresh<F>(provider: &str, future: F) -> Result<F::Output>
where
    F: std::future::Future,
{
    tokio::time::timeout(TOKEN_REFRESH_TIMEOUT, future)
        .await
        .map_err(|_| {
            anyhow!(
                "{} OAuth token refresh timed out after {}s",
                provider,
                TOKEN_REFRESH_TIMEOUT.as_secs()
            )
        })
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
    let tokens = read_stored_tokens(provider, store).await?;
    refresh_with_tokens(provider, store, &tokens).await
}

/// Read stored tokens, reporting keyring failures distinctly from "not logged in".
async fn read_stored_tokens(provider: &dyn OAuthProvider, store: &SecretStore) -> Result<TokenSet> {
    let name = provider.name().to_owned();
    store
        .run(move |store| store.try_get_oauth_tokens(&name))
        .await
        .map_err(|e| {
            anyhow!(
                "Keyring access failed while reading {} OAuth tokens: {}",
                provider.display_name(),
                e
            )
        })?
        .ok_or_else(|| anyhow!("No OAuth tokens found for {}", provider.display_name()))
}

/// Result of a successful remote refresh, before it is persisted.
struct RefreshedTokens {
    tokens: TokenSet,
    api_key: Option<String>,
}

/// Exchange `refresh_token` for new tokens (and optionally an API key).
///
/// Bounded so a stalled transport releases the per-provider refresh lock
/// promptly instead of blocking every later token read. Performs no keyring
/// access.
async fn refresh_remote(
    provider: &dyn OAuthProvider,
    refresh_token: &str,
) -> Result<RefreshedTokens> {
    bound_refresh(provider.display_name(), async {
        let tokens = provider.refresh_token(refresh_token).await?;
        let api_key = match provider.create_api_key(&tokens.access_token).await {
            Ok(key) => key,
            Err(error) => {
                log::debug!(
                    "{} API key creation after refresh failed: {}",
                    provider.display_name(),
                    error
                );
                None
            }
        };
        Ok::<RefreshedTokens, anyhow::Error>(RefreshedTokens { tokens, api_key })
    })
    .await?
}

/// Persist refreshed tokens (and API key, best effort).
///
/// Only a failure to store the token set is reported; the API key is a
/// convenience copy.
fn persist_refreshed(
    store: &mut SecretStore,
    provider_name: &str,
    api_key_name: Option<&str>,
    refreshed: &RefreshedTokens,
) -> std::io::Result<()> {
    store.set_oauth_tokens(provider_name, &refreshed.tokens)?;
    if let (Some(api_key), Some(key_name)) = (refreshed.api_key.as_deref(), api_key_name)
        && let Err(error) = store.set(key_name, api_key)
    {
        log::warn!("Failed to persist API key '{key_name}' after token refresh: {error}");
    }
    Ok(())
}

/// Refresh `tokens` and persist the result.
///
/// Refresh tokens rotate, so once the remote exchange has succeeded the new
/// tokens are returned even if the keyring write fails; discarding them would
/// leave only an already-invalidated refresh token on disk.
async fn refresh_with_tokens(
    provider: &dyn OAuthProvider,
    store: &mut SecretStore,
    tokens: &TokenSet,
) -> Result<TokenSet> {
    let refreshed = refresh_remote(provider, &tokens.refresh_token).await?;
    let tokens = refreshed.tokens.clone();
    let name = provider.name().to_owned();
    let key_name = provider.api_key_name().map(str::to_owned);
    if let Err(error) = store
        .run(move |store| persist_refreshed(store, &name, key_name.as_deref(), &refreshed))
        .await
    {
        log::warn!(
            "Refreshed {} OAuth tokens but could not save them to the keyring: {}. \
             Using them for this session only.",
            provider.display_name(),
            error
        );
    }
    Ok(tokens)
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
    // Single keyring read; the same token set feeds the refresh if needed.
    let tokens = read_stored_tokens(provider, store).await?;
    if !tokens.is_expired() {
        return Ok(tokens.access_token);
    }

    log::info!(
        "{} OAuth token expired, attempting to refresh...",
        provider.display_name()
    );
    let new_tokens = refresh_with_tokens(provider, store, &tokens).await?;

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

        let name = p.clone();
        if let Some(tokens) = store
            .run(move |store| Ok(store.get_oauth_tokens(&name)))
            .await?
        {
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

    let tokens =
        timeout_callback_result(timeout, run_callback_server(port, state, &client, verifier))
            .await?;

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

    let callback = timeout_callback_result(timeout, run_callback_server(port, state)).await?;

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
/// # Errors
///
/// Fails with "No OAuth tokens found" when the user is not logged in, and with
/// a keyring error (backed off for 30s) when the keyring cannot be read.
///
/// When tokens are stored but cannot be refreshed the error is a
/// [`TokenRefreshError`] (see [`token_refresh_error`]):
/// [`relogin_required`](TokenRefreshError::relogin_required) is true when the
/// server rejected the refresh token, false for transient failures. The failure
/// is logged once and remembered: later calls return it immediately, without
/// contacting the server, until the stored refresh token changes (re-login or
/// rotation by another process) or, for transient failures, a 15s back-off
/// ends.
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
    // Owned guard: the final keyring write below moves it into the blocking
    // task, so the lock outlives this future if it is cancelled mid-persist.
    let guard = lock_provider_tokens(provider).await;

    // Fast path: a token cached by this process that is still comfortably valid
    // needs no keyring access (and therefore no keychain prompt).
    if let Some(token) = token_cache::fresh_access_token(provider) {
        return Ok(token);
    }

    let oauth_provider = get_oauth_provider(provider, None)?;
    let display = oauth_provider.display_name().to_string();

    // After a recent keyring failure (denied/cancelled prompt, locked keychain),
    // fail fast instead of re-prompting on every LLM step.
    if let Some(remaining) = token_cache::failure_backoff_remaining(provider) {
        return Err(anyhow!(
            "{display} OAuth tokens are unavailable: the system keyring denied or failed a \
             recent read. Retrying in {}s. Check that this binary is allowed to access the \
             'querymt' keychain item.",
            remaining.as_secs().max(1)
        ));
    }

    // Load credentials only after acquiring the guard: another process may have
    // rotated them. The keyring call can block on a user prompt, so it runs off
    // the async worker threads.
    log::debug!("Checking OAuth tokens for provider: {}", provider);
    let name = provider.to_string();
    let stored = keyring_blocking(move |store| {
        store
            .try_get_oauth_tokens(&name)
            .map_err(anyhow::Error::from)
    })
    .await;

    let tokens = match stored {
        Ok(Some(tokens)) => tokens,
        Ok(None) => return Err(anyhow!("No OAuth tokens found for {display}")),
        Err(error) => {
            token_cache::record_failure(provider);
            log::warn!(
                "Keyring access failed while reading {display} OAuth tokens: {error}. \
                 Not retrying for {}s.",
                token_cache::KEYRING_FAILURE_BACKOFF.as_secs()
            );
            return Err(anyhow!(
                "Keyring access failed while reading {display} OAuth tokens: {error}"
            ));
        }
    };

    let provider_key = provider.to_string();
    let resolved = resolve_tokens(provider, oauth_provider.as_ref(), tokens, || {
        let name = provider_key.clone();
        keyring_blocking(move |store| {
            store
                .try_get_oauth_tokens(&name)
                .map_err(anyhow::Error::from)
        })
    })
    .await
    .map_err(anyhow::Error::new)?;
    let refreshed = match resolved {
        Resolved::Valid(tokens) => {
            let access_token = tokens.access_token.clone();
            token_cache::store(provider, tokens);
            return Ok(access_token);
        }
        Resolved::Refreshed(refreshed) => refreshed,
    };

    // Refresh tokens rotate: keep the new set in memory before touching the
    // keyring so a failed write cannot lose it.
    let access_token = refreshed.tokens.access_token.clone();
    token_cache::store(provider, refreshed.tokens.clone());

    let name = provider.to_string();
    let api_key_name = oauth_provider.api_key_name().map(str::to_owned);
    // `spawn_blocking` work cannot be cancelled: if this future is dropped while
    // the write is in flight, the write still completes. Hand the guard to the
    // blocking task so logout/login cannot interleave with that late write.
    let persisted = run_blocking_holding(guard, move || {
        let mut store = SecretStore::new().map_err(|e| anyhow!("{e}"))?;
        persist_refreshed(&mut store, &name, api_key_name.as_deref(), &refreshed)
            .map_err(anyhow::Error::from)
    })
    .await;
    if let Err(error) = persisted {
        log::warn!(
            "Refreshed {display} OAuth tokens but could not save them to the keyring: {error}. \
             Using them for this session only."
        );
    }

    Ok(access_token)
}

/// Outcome of [`resolve_tokens`].
enum Resolved {
    /// The stored tokens are still valid.
    Valid(TokenSet),
    /// The stored tokens were expired and have been refreshed (not yet persisted).
    Refreshed(RefreshedTokens),
}

/// Return usable tokens for `provider`, refreshing `tokens` when expired.
///
/// A refresh token the server has rejected is not retried until the stored
/// token changes. On a rejection the keyring is re-read once through
/// `reread_keyring`: another process may already have rotated the token, in
/// which case the new one is used. The failure is logged here, once, when it is
/// first recorded.
async fn resolve_tokens<F, Fut>(
    provider: &str,
    oauth_provider: &dyn OAuthProvider,
    mut tokens: TokenSet,
    mut reread_keyring: F,
) -> std::result::Result<Resolved, TokenRefreshError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<TokenSet>>>,
{
    let display = oauth_provider.display_name();
    let mut reread_after_rejection = false;
    loop {
        if !tokens.is_expired() {
            return Ok(Resolved::Valid(tokens));
        }

        // Do not hammer the token endpoint with a refresh token that was just
        // rejected; it stays rejected until the stored token changes.
        if let Some(error) = token_cache::refresh_failure(provider, &tokens.refresh_token) {
            return Err(error);
        }

        log::info!("{display} OAuth token expired, attempting to refresh...");
        let error = match refresh_remote(oauth_provider, &tokens.refresh_token).await {
            Ok(refreshed) => return Ok(Resolved::Refreshed(refreshed)),
            Err(error) => classify_refresh_error(display, error),
        };

        if error.relogin_required() && !reread_after_rejection {
            reread_after_rejection = true;
            match reread_keyring().await {
                Ok(Some(latest)) if latest.refresh_token != tokens.refresh_token => {
                    log::info!("{display} refresh token was rotated by another process; retrying");
                    tokens = latest;
                    continue;
                }
                Ok(_) => {}
                Err(reread_error) => log::debug!(
                    "Could not re-read {display} OAuth tokens after a rejected refresh: {reread_error}"
                ),
            }
        }

        log::warn!(
            "{display} OAuth token refresh failed (relogin_required={}): {}",
            error.relogin_required(),
            error.detail()
        );
        token_cache::record_refresh_failure(provider, &tokens.refresh_token, error.clone());
        return Err(error);
    }
}

/// Turn any refresh failure into a [`TokenRefreshError`].
fn classify_refresh_error(display: &str, error: anyhow::Error) -> TokenRefreshError {
    let error = match error.downcast::<TokenRefreshError>() {
        Ok(typed) => return typed,
        Err(error) => error,
    };
    match error.downcast::<XaiOAuthError>() {
        Ok(XaiOAuthError::RefreshFailed {
            message,
            relogin_required: true,
        }) => TokenRefreshError::relogin(display, message),
        Ok(XaiOAuthError::RefreshFailed { message, .. }) => {
            TokenRefreshError::failed(display, message)
        }
        Ok(other) => TokenRefreshError::failed(display, other.to_string()),
        Err(error) => TokenRefreshError::failed(display, error.to_string()),
    }
}

/// Acquire the per-provider OAuth token lock shared with [`get_or_refresh_token`].
///
/// Hold the returned guard while writing or deleting a provider's stored
/// credentials (login/logout) so an in-flight refresh cannot repopulate the
/// cache or keyring afterwards. Do not call [`get_or_refresh_token`] for the
/// same provider while holding it.
pub async fn lock_provider_tokens(provider: &str) -> tokio::sync::OwnedMutexGuard<()> {
    oauth_refresh_lock(provider).await.lock_owned().await
}

/// Drop any token or keyring back-off state cached for `provider` in this process.
///
/// Call after logging in or out so the next [`get_or_refresh_token`] re-reads
/// the keyring.
pub fn invalidate_cached_tokens(provider: &str) {
    token_cache::clear(provider);
}

/// Run `operation` on the blocking thread pool, keeping `guard` held until it
/// finishes.
///
/// Unlike a guard held by the awaiting future, this one is released only when
/// the blocking work itself completes, even if the caller is cancelled.
async fn run_blocking_holding<T, F>(
    guard: tokio::sync::OwnedMutexGuard<()>,
    operation: F,
) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        operation()
    })
    .await
    .map_err(|e| anyhow!("keyring task failed: {e}"))?
}

/// Run a keyring operation on the blocking thread pool.
///
/// Keychain access is synchronous and may wait on a user prompt, which must not
/// stall an async worker thread.
async fn keyring_blocking<T, F>(operation: F) -> Result<T>
where
    F: FnOnce(&mut SecretStore) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut store = SecretStore::new().map_err(|e| anyhow!("{e}"))?;
        operation(&mut store)
    })
    .await
    .map_err(|e| anyhow!("keyring task failed: {e}"))?
}

/// Per-process token cache and keyring failure back-off, keyed by provider.
mod token_cache {
    use super::{TokenRefreshError, TokenSet};
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::{Duration, Instant};

    /// How long to avoid the keyring after a failed read.
    pub(super) const KEYRING_FAILURE_BACKOFF: Duration = Duration::from_secs(30);

    /// How long a transient refresh failure suppresses further attempts.
    pub(super) const REFRESH_FAILURE_BACKOFF: Duration = Duration::from_secs(15);

    /// A failed refresh, tied to the refresh token that failed.
    struct RefreshFailure {
        fingerprint: u64,
        at: Instant,
        error: TokenRefreshError,
    }

    fn fingerprint(refresh_token: &str) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        refresh_token.hash(&mut hasher);
        hasher.finish()
    }

    #[derive(Default)]
    struct State {
        tokens: HashMap<String, TokenSet>,
        failures: HashMap<String, Instant>,
        refresh_failures: HashMap<String, RefreshFailure>,
    }

    fn state() -> MutexGuard<'static, State> {
        static STATE: OnceLock<Mutex<State>> = OnceLock::new();
        STATE
            .get_or_init(|| Mutex::new(State::default()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Cached access token, if it is not within the expiry buffer.
    pub(super) fn fresh_access_token(provider: &str) -> Option<String> {
        state()
            .tokens
            .get(provider)
            .filter(|tokens| !tokens.is_expired())
            .map(|tokens| tokens.access_token.clone())
    }

    pub(super) fn store(provider: &str, tokens: TokenSet) {
        let mut state = state();
        state.failures.remove(provider);
        state.refresh_failures.remove(provider);
        state.tokens.insert(provider.to_string(), tokens);
    }

    pub(super) fn record_refresh_failure(
        provider: &str,
        refresh_token: &str,
        error: TokenRefreshError,
    ) {
        state().refresh_failures.insert(
            provider.to_string(),
            RefreshFailure {
                fingerprint: fingerprint(refresh_token),
                at: Instant::now(),
                error,
            },
        );
    }

    /// The recorded failure for this exact refresh token, if it still applies.
    ///
    /// A rejected token (`relogin_required`) stays failed until the stored token
    /// changes; other failures expire after [`REFRESH_FAILURE_BACKOFF`].
    pub(super) fn refresh_failure(
        provider: &str,
        refresh_token: &str,
    ) -> Option<TokenRefreshError> {
        refresh_failure_at(provider, refresh_token, Instant::now())
    }

    /// [`refresh_failure`] evaluated at `now`, so back-off expiry can be tested
    /// without sleeping.
    pub(super) fn refresh_failure_at(
        provider: &str,
        refresh_token: &str,
        now: Instant,
    ) -> Option<TokenRefreshError> {
        let mut state = state();
        let failure = state.refresh_failures.get(provider)?;
        let stale = failure.fingerprint != fingerprint(refresh_token);
        let relogin = failure.error.relogin_required();
        let remaining = REFRESH_FAILURE_BACKOFF
            .checked_sub(now.saturating_duration_since(failure.at))
            .filter(|remaining| !remaining.is_zero());
        let error = failure.error.clone();
        if stale || (!relogin && remaining.is_none()) {
            state.refresh_failures.remove(provider);
            return None;
        }
        Some(match remaining {
            Some(remaining) if !relogin => error.retrying_in(remaining),
            _ => error,
        })
    }

    pub(super) fn record_failure(provider: &str) {
        state()
            .failures
            .insert(provider.to_string(), Instant::now());
    }

    /// Remaining back-off after a keyring failure, if still active.
    pub(super) fn failure_backoff_remaining(provider: &str) -> Option<Duration> {
        let mut state = state();
        let failed_at = *state.failures.get(provider)?;
        match KEYRING_FAILURE_BACKOFF.checked_sub(failed_at.elapsed()) {
            Some(remaining) if !remaining.is_zero() => Some(remaining),
            _ => {
                state.failures.remove(provider);
                None
            }
        }
    }

    pub(super) fn clear(provider: &str) {
        let mut state = state();
        state.tokens.remove(provider);
        state.failures.remove(provider);
        state.refresh_failures.remove(provider);
    }
}

async fn oauth_refresh_lock(provider: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock};
    type RefreshLocks = tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;
    static LOCKS: OnceLock<RefreshLocks> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(RefreshLocks::default).lock().await;
    locks.entry(provider.to_string()).or_default().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_flow_kinds() {
        let anthropic = AnthropicProvider::new(OAuthMode::Max);
        assert_eq!(anthropic.flow_kind(), OAuthFlowKind::RedirectCode);
        assert_eq!(anthropic.callback_port(), None);
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
        let verifier = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
        let challenge = XaiProvider::code_challenge(verifier);
        let url = XaiProvider::build_authorization_url(
            XaiProvider::DEFAULT_AUTH_URL,
            XaiProvider::DEFAULT_CLIENT_ID,
            XaiProvider::DEFAULT_REDIRECT_URI,
            state,
            nonce,
            &challenge,
        )
        .unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();

        assert_eq!(
            parsed.as_str().split('?').next(),
            Some(XaiProvider::DEFAULT_AUTH_URL)
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
            Some(challenge.as_str())
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
            token_endpoint: "https://auth.x.ai/oauth2/token".to_string(),
            redirect_uri: XaiProvider::DEFAULT_REDIRECT_URI.to_string(),
            client_id: XaiProvider::DEFAULT_CLIENT_ID.to_string(),
        };
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let decoded: XaiFlowSnapshot = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded.code_verifier, "verifier");
        assert!(!decoded.code_challenge.is_empty());
        assert_eq!(decoded.token_endpoint, "https://auth.x.ai/oauth2/token");
    }

    #[test]
    fn xai_exchange_form_echoes_pkce_challenge() {
        let snapshot = XaiFlowSnapshot {
            code_verifier: "verifier".to_string(),
            code_challenge: "challenge".to_string(),
            token_endpoint: "https://auth.x.ai/oauth2/token".to_string(),
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
    fn xai_pkce_challenge_matches_verifier_across_authorize_and_exchange() {
        let code_verifier = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
        let code_challenge = XaiProvider::code_challenge(code_verifier);
        let authorize_url = XaiProvider::build_authorization_url(
            XaiProvider::DEFAULT_AUTH_URL,
            XaiProvider::DEFAULT_CLIENT_ID,
            XaiProvider::DEFAULT_REDIRECT_URI,
            "state-value",
            "nonce-value",
            &code_challenge,
        )
        .unwrap();
        let authorize_params: std::collections::HashMap<_, _> = url::Url::parse(&authorize_url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let snapshot = XaiFlowSnapshot {
            code_verifier: code_verifier.to_string(),
            code_challenge: code_challenge.clone(),
            token_endpoint: XaiProvider::DEFAULT_TOKEN_URL.to_string(),
            redirect_uri: XaiProvider::DEFAULT_REDIRECT_URI.to_string(),
            client_id: XaiProvider::DEFAULT_CLIENT_ID.to_string(),
        };
        let exchange_form: std::collections::HashMap<_, _> =
            XaiProvider::exchange_form("auth-code", &snapshot)
                .into_iter()
                .collect();

        assert_eq!(
            authorize_params.get("code_challenge").map(String::as_str),
            Some(code_challenge.as_str())
        );
        assert_eq!(exchange_form.get("code_verifier"), Some(&code_verifier));
        assert_eq!(
            exchange_form.get("code_challenge"),
            Some(&code_challenge.as_str())
        );
    }

    #[test]
    fn xai_refresh_preserves_existing_refresh_token_when_omitted() {
        let mut response = XaiTokenResponse::new(
            oauth2::AccessToken::new("access".to_string()),
            oauth2::basic::BasicTokenType::Bearer,
            oauth2::EmptyExtraTokenFields {},
        );
        response.set_expires_in(Some(&Duration::from_secs(3600)));
        let tokens = XaiProvider::token_set(response, Some("old-refresh")).unwrap();

        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token, "old-refresh");
        assert!(tokens.expires_at > current_epoch_seconds());
    }

    #[test]
    fn get_oauth_provider_rejects_unknown() {
        assert!(get_oauth_provider("unknown-provider", None).is_err());
    }

    fn token_set(expires_in_secs: u64) -> TokenSet {
        TokenSet {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            expires_at: current_epoch_seconds() + expires_in_secs,
        }
    }

    #[test]
    fn token_cache_serves_only_fresh_tokens() {
        let provider = "cache-test-fresh";
        assert!(token_cache::fresh_access_token(provider).is_none());

        token_cache::store(provider, token_set(3600));
        assert_eq!(
            token_cache::fresh_access_token(provider).as_deref(),
            Some("access")
        );

        // Inside the expiry buffer the cache must defer to the keyring, which
        // may hold a newer rotated token set.
        token_cache::store(provider, token_set(10));
        assert!(token_cache::fresh_access_token(provider).is_none());
        token_cache::clear(provider);
    }

    #[test]
    fn keyring_failure_backoff_is_active_until_cleared_or_token_stored() {
        let provider = "cache-test-backoff";
        assert!(token_cache::failure_backoff_remaining(provider).is_none());

        token_cache::record_failure(provider);
        let remaining = token_cache::failure_backoff_remaining(provider).expect("active");
        assert!(remaining <= token_cache::KEYRING_FAILURE_BACKOFF);

        invalidate_cached_tokens(provider);
        assert!(token_cache::failure_backoff_remaining(provider).is_none());

        token_cache::record_failure(provider);
        token_cache::store(provider, token_set(3600));
        assert!(token_cache::failure_backoff_remaining(provider).is_none());
        token_cache::clear(provider);
    }

    #[tokio::test]
    async fn provider_lock_outlives_cancelled_caller_until_blocking_work_finishes() {
        use std::time::Duration;
        let provider = "lock-outlives-cancel-test";

        let guard = lock_provider_tokens(provider).await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

        let caller = tokio::spawn(run_blocking_holding(guard, move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            Ok(())
        }));
        started_rx.await.expect("blocking work started");

        // Cancel the awaiting future while the blocking work is in flight.
        caller.abort();
        let _ = caller.await;

        // The orphaned blocking task must still own the lock.
        let contended =
            tokio::time::timeout(Duration::from_millis(100), lock_provider_tokens(provider)).await;
        assert!(
            contended.is_err(),
            "lock released before blocking work finished"
        );

        release_tx.send(()).expect("release blocking work");
        tokio::time::timeout(Duration::from_secs(5), lock_provider_tokens(provider))
            .await
            .expect("lock released after blocking work finished");
    }

    #[tokio::test]
    async fn get_or_refresh_token_uses_cache_without_keyring() {
        // "anthropic" is a known provider; a cached fresh token must be returned
        // before any keyring access is attempted.
        token_cache::store("anthropic", token_set(3600));
        let token = get_or_refresh_token("anthropic").await.expect("cached");
        assert_eq!(token, "access");
        token_cache::clear("anthropic");
    }

    #[tokio::test]
    async fn get_or_refresh_token_fails_fast_during_backoff() {
        token_cache::record_failure("codex");
        let error = get_or_refresh_token("codex").await.unwrap_err().to_string();
        assert!(error.contains("unavailable"), "{error}");
        token_cache::clear("codex");
    }

    #[test]
    fn rejected_refresh_token_stays_failed_until_it_changes() {
        let provider = "refresh-failure-test";
        let error = TokenRefreshError::relogin("Test", "invalid_grant");
        token_cache::record_refresh_failure(provider, "old", error);

        let hit = token_cache::refresh_failure(provider, "old").expect("still failed");
        assert!(hit.relogin_required());
        assert!(hit.to_string().contains("sign in again"));

        // A different refresh token (re-login or rotation elsewhere) clears it.
        assert!(token_cache::refresh_failure(provider, "new").is_none());
        assert!(token_cache::refresh_failure(provider, "old").is_none());
    }

    #[test]
    fn store_and_clear_reset_refresh_failure() {
        let provider = "refresh-failure-reset-test";
        for reset in [true, false] {
            token_cache::record_refresh_failure(
                provider,
                "old",
                TokenRefreshError::relogin("Test", "invalid_grant"),
            );
            if reset {
                token_cache::store(provider, token_set(3600));
            } else {
                token_cache::clear(provider);
            }
            assert!(token_cache::refresh_failure(provider, "old").is_none());
        }
        token_cache::clear(provider);
    }

    #[test]
    fn transient_refresh_failure_is_not_permanent() {
        let provider = "refresh-transient-test";
        token_cache::record_refresh_failure(
            provider,
            "tok",
            TokenRefreshError::failed("Test", "timed out"),
        );
        let hit = token_cache::refresh_failure(provider, "tok").expect("within back-off");
        assert!(!hit.relogin_required());
        // The remaining back-off is reported, like the keyring back-off message.
        assert!(hit.to_string().contains("retrying in"), "{hit}");

        // Still blocked just before the back-off ends, retryable right after.
        let failed_at = std::time::Instant::now();
        let just_before = failed_at + token_cache::REFRESH_FAILURE_BACKOFF - Duration::from_secs(1);
        assert!(token_cache::refresh_failure_at(provider, "tok", just_before).is_some());
        let after = failed_at + token_cache::REFRESH_FAILURE_BACKOFF + Duration::from_secs(1);
        assert!(token_cache::refresh_failure_at(provider, "tok", after).is_none());
        // The expired entry is dropped.
        assert!(token_cache::refresh_failure(provider, "tok").is_none());
        token_cache::clear(provider);
    }

    #[test]
    fn rejected_refresh_token_is_not_released_by_elapsed_time() {
        let provider = "refresh-rejected-time-test";
        token_cache::record_refresh_failure(
            provider,
            "tok",
            TokenRefreshError::relogin("Test", "invalid_grant"),
        );
        let later = std::time::Instant::now()
            + token_cache::REFRESH_FAILURE_BACKOFF
            + Duration::from_secs(60);
        assert!(token_cache::refresh_failure_at(provider, "tok", later).is_some());
        token_cache::clear(provider);
    }

    #[test]
    fn classify_refresh_error_preserves_typed_errors() {
        let typed = anyhow::Error::new(TokenRefreshError::relogin("Anthropic", "invalid_grant"));
        assert!(classify_refresh_error("Anthropic", typed).relogin_required());
        let other = classify_refresh_error("Anthropic", anyhow!("connection reset"));
        assert!(!other.relogin_required());
        assert!(other.detail().contains("connection reset"));
        for (relogin, expected) in [(true, true), (false, false)] {
            let xai = anyhow::Error::new(XaiOAuthError::RefreshFailed {
                message: "bad".into(),
                relogin_required: relogin,
            });
            let classified = classify_refresh_error("xAI", xai);
            assert_eq!(classified.relogin_required(), expected);
            assert_eq!(classified.detail(), "bad");
        }
    }

    #[test]
    fn anthropic_credential_rejection_requires_relogin() {
        use anthropic_auth::AnthropicAuthError::Http;
        // The library renders the body as `HTTP <status> - <raw>\nHint: ...`.
        let rejection = |status| Http {
            status,
            body: format!(
                "HTTP {status} - {{\"error\":\"invalid_grant\",\"error_description\":\"expired\"}}\nHint: retry"
            ),
        };
        for status in [400, 401, 403] {
            let error = map_anthropic_refresh_error("Anthropic", rejection(status));
            let typed = token_refresh_error(&error).expect("typed refresh error");
            assert!(typed.relogin_required(), "status {status}");
            assert_eq!(typed.provider(), "Anthropic");
            assert_eq!(typed.detail(), "invalid_grant");
        }
        // Not a refresh rejection: statuses outside 400/401/403, a proxy error
        // page, or an unrelated 4xx error code.
        let others = [
            Http {
                status: 429,
                body: "busy".into(),
            },
            Http {
                status: 503,
                body: "{\"error\":\"invalid_grant\"}".into(),
            },
            Http {
                status: 403,
                body: "HTTP 403 - <html>Forbidden</html>".into(),
            },
            Http {
                status: 400,
                body: "HTTP 400 - {\"error\":\"invalid_request\"}".into(),
            },
        ];
        for other in others {
            let error = map_anthropic_refresh_error("Anthropic", other);
            assert!(token_refresh_error(&error).is_none());
        }
    }

    #[test]
    fn refresh_rejection_code_reads_known_codes_only() {
        assert_eq!(
            refresh_rejection_code("{\"error\":\"invalid_grant\"}"),
            Some("invalid_grant")
        );
        assert_eq!(
            refresh_rejection_code("{\"error\":{\"code\":\"refresh_token_expired\"}}"),
            Some("refresh_token_expired")
        );
        assert_eq!(
            refresh_rejection_code("{\"error\":{\"type\":\"invalid_grant\"}}"),
            Some("invalid_grant")
        );
        assert_eq!(refresh_rejection_code("{\"error\":\"server_error\"}"), None);
        assert_eq!(refresh_rejection_code("<html>invalid_grant</html>"), None);
        assert_eq!(refresh_rejection_code("} not json {"), None);
        assert_eq!(refresh_rejection_code(""), None);
    }

    #[test]
    fn codex_refresh_rejection_requires_relogin_but_outages_do_not() {
        use openai_auth::OpenAIAuthError::{ApiKeyExchange, Http};
        let body = "{\"error\":{\"code\":\"refresh_token_expired\"}}";
        for status in [400, 401] {
            let error = map_codex_refresh_error(
                "Codex",
                ApiKeyExchange {
                    status,
                    body: body.into(),
                },
            );
            let typed = token_refresh_error(&error).expect("typed refresh error");
            assert!(typed.relogin_required(), "status {status}");
            assert_eq!(typed.detail(), "refresh_token_expired");
        }
        let error = map_codex_refresh_error(
            "Codex",
            Http {
                status: 400,
                body: body.into(),
            },
        );
        assert!(token_refresh_error(&error).is_some());
        // A server outage, even with a rejection-looking body, stays transient.
        let error = map_codex_refresh_error(
            "Codex",
            ApiKeyExchange {
                status: 503,
                body: body.into(),
            },
        );
        assert!(token_refresh_error(&error).is_none());
        let error = map_codex_refresh_error(
            "Codex",
            ApiKeyExchange {
                status: 400,
                body: "{\"error\":\"other\"}".into(),
            },
        );
        assert!(token_refresh_error(&error).is_none());
    }

    #[test]
    fn anthropic_refresh_retries_only_transient_errors() {
        use anthropic_auth::AnthropicAuthError::Http;
        let http = |status| Http {
            status,
            body: String::new(),
        };
        assert!(anthropic_refresh_retryable(&http(503)));
        assert!(anthropic_refresh_retryable(&http(429)));
        assert!(!anthropic_refresh_retryable(&http(400)));
        assert!(!anthropic_refresh_retryable(&http(401)));
    }

    /// What a [`FakeProvider`] answers to one refresh call.
    enum FakeRefresh {
        Tokens(TokenSet),
        Rejected,
        Transient,
    }

    struct FakeProvider {
        answers: std::sync::Mutex<std::collections::VecDeque<FakeRefresh>>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeProvider {
        fn new(answers: impl IntoIterator<Item = FakeRefresh>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers.into_iter().collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl OAuthProvider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }
        fn display_name(&self) -> &str {
            "Fake"
        }
        async fn start_flow(&self) -> Result<OAuthFlowData> {
            Err(anyhow!("unused"))
        }
        async fn exchange_code(&self, _: &str, _: &str, _: &str) -> Result<TokenSet> {
            Err(anyhow!("unused"))
        }
        async fn refresh_token(&self, _refresh_token: &str) -> Result<TokenSet> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let answer = self.answers.lock().unwrap().pop_front();
            match answer.expect("unexpected refresh call") {
                FakeRefresh::Tokens(tokens) => Ok(tokens),
                FakeRefresh::Rejected => Err(anyhow::Error::new(TokenRefreshError::relogin(
                    "Fake",
                    "invalid_grant",
                ))),
                FakeRefresh::Transient => Err(anyhow!("connection reset")),
            }
        }
        async fn create_api_key(&self, _: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn api_key_name(&self) -> Option<&str> {
            None
        }
    }

    fn expired(refresh_token: &str) -> TokenSet {
        TokenSet {
            access_token: "stale".into(),
            refresh_token: refresh_token.into(),
            expires_at: 0,
        }
    }

    fn fresh(access_token: &str, refresh_token: &str) -> TokenSet {
        TokenSet {
            access_token: access_token.into(),
            refresh_token: refresh_token.into(),
            expires_at: current_epoch_seconds() + 3600,
        }
    }

    #[tokio::test]
    async fn resolve_tokens_returns_valid_tokens_without_refreshing() {
        let provider = FakeProvider::new([]);
        let resolved = resolve_tokens("resolve-valid-test", &provider, fresh("live", "r"), || {
            std::future::ready(Ok(None))
        })
        .await
        .unwrap();
        assert!(matches!(resolved, Resolved::Valid(t) if t.access_token == "live"));
        assert_eq!(provider.calls(), 0);
    }

    #[tokio::test]
    async fn resolve_tokens_uses_token_rotated_by_another_process() {
        let key = "resolve-rotated-test";
        let provider = FakeProvider::new([
            FakeRefresh::Rejected,
            FakeRefresh::Tokens(fresh("new-access", "new-refresh")),
        ]);
        let rotated = expired("rotated-elsewhere");
        let resolved = resolve_tokens(key, &provider, expired("old"), || {
            std::future::ready(Ok(Some(rotated.clone())))
        })
        .await
        .unwrap();
        assert!(
            matches!(resolved, Resolved::Refreshed(r) if r.tokens.access_token == "new-access")
        );
        assert_eq!(provider.calls(), 2);
        token_cache::clear(key);
    }

    #[tokio::test]
    async fn resolve_tokens_remembers_a_rejected_refresh_token() {
        let key = "resolve-rejected-test";
        let provider = FakeProvider::new([FakeRefresh::Rejected]);
        let same = expired("old");
        let error = match resolve_tokens(key, &provider, expired("old"), || {
            std::future::ready(Ok(Some(same.clone())))
        })
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("a rejected refresh must fail"),
        };
        assert!(error.relogin_required());
        assert_eq!(provider.calls(), 1);

        // The same refresh token fails fast, without contacting the server.
        let again = resolve_tokens(key, &provider, expired("old"), || {
            std::future::ready(Ok(None))
        })
        .await;
        assert!(matches!(again, Err(e) if e.relogin_required()));
        assert_eq!(provider.calls(), 1);

        // A new refresh token (re-login) is attempted again.
        let provider = FakeProvider::new([FakeRefresh::Tokens(fresh("a", "r2"))]);
        let relogged = resolve_tokens(key, &provider, expired("new"), || {
            std::future::ready(Ok(None))
        })
        .await;
        assert!(matches!(relogged, Ok(Resolved::Refreshed(_))));
        token_cache::clear(key);
    }

    #[tokio::test]
    async fn resolve_tokens_does_not_reread_keyring_for_transient_failures() {
        let key = "resolve-transient-test";
        let provider = FakeProvider::new([FakeRefresh::Transient]);
        let rereads = std::sync::atomic::AtomicUsize::new(0);
        let result = resolve_tokens(key, &provider, expired("old"), || {
            rereads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(None))
        })
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a failed refresh must fail"),
        };
        assert!(!error.relogin_required());
        assert!(error.detail().contains("connection reset"));
        assert_eq!(rereads.load(std::sync::atomic::Ordering::SeqCst), 0);
        token_cache::clear(key);
    }

    #[tokio::test]
    async fn resolve_tokens_survives_a_failing_keyring_reread() {
        let key = "resolve-reread-error-test";
        let provider = FakeProvider::new([FakeRefresh::Rejected]);
        let result = resolve_tokens(key, &provider, expired("old"), || {
            std::future::ready(Err(anyhow!("keychain denied")))
        })
        .await;
        assert!(matches!(result, Err(e) if e.relogin_required()));
        token_cache::clear(key);
    }

    #[tokio::test(start_paused = true)]
    async fn token_refresh_deadline_bounds_hanging_refresh() {
        // A refresh future that never completes must surface a timeout error
        // instead of holding the per-provider refresh lock forever.
        let result = bound_refresh("test-provider", std::future::pending::<()>()).await;
        let err = result.expect_err("hanging refresh should time out");
        let message = err.to_string();
        assert!(message.contains("test-provider"), "message: {message}");
        assert!(message.contains("timed out"), "message: {message}");
    }
}
