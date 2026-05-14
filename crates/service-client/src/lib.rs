//! QueryMT service OAuth and API client primitives.
//!
//! This crate is intentionally isolated from provider OAuth. It owns QueryMT
//! service token shapes and persistence keys without registering the service as
//! an LLM provider credential.

use oauth2::basic::{BasicClient, BasicTokenResponse, BasicTokenType};
use oauth2::reqwest;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, RefreshToken, RequestTokenError, Scope, TokenResponse, TokenUrl,
};
use querymt_utils::secret_store::SecretStore;
use serde::{Deserialize, Serialize};
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time;
use url::Url;

/// Environment variable used to override the QueryMT service endpoint.
pub const QUERYMT_SERVICE_ENDPOINT_ENV: &str = "QUERYMT_SERVICE_ENDPOINT";

/// Compiled fallback endpoint for QueryMT service requests.
///
/// This placeholder is centralized so the real production endpoint can be
/// swapped in without changing endpoint resolution call sites.
pub const DEFAULT_SERVICE_ENDPOINT: &str = "https://service.querymt.invalid/";

/// Boruta authorization endpoint path under the resolved QueryMT service endpoint.
pub const SERVICE_OAUTH_AUTHORIZE_PATH: &str = "/oauth/authorize";
/// Future token exchange endpoint path under the resolved QueryMT service endpoint.
pub const SERVICE_OAUTH_TOKEN_PATH: &str = "/oauth/token";
/// OAuth token revocation endpoint path under the resolved QueryMT service endpoint.
pub const SERVICE_OAUTH_REVOKE_PATH: &str = "/oauth/revoke";
/// Service status API endpoint path under the resolved QueryMT service endpoint.
pub const SERVICE_API_STATUS_PATH: &str = "/api/service/status";

/// Default loopback host for the service OAuth callback listener.
pub const SERVICE_OAUTH_REDIRECT_HOST: &str = "127.0.0.1";
/// Default loopback port for the service OAuth callback listener.
pub const SERVICE_OAUTH_REDIRECT_PORT: u16 = 1455;
/// Default loopback path for the service OAuth callback listener.
pub const SERVICE_OAUTH_REDIRECT_PATH: &str = "/auth/callback";
/// Default loopback redirect URI registered for the service OAuth client.
pub const SERVICE_OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";

/// Default service scopes requested during login until the service contract is finalized.
pub const DEFAULT_SERVICE_OAUTH_SCOPES: &[&str] = &["service:status"];

/// SecretStore key for the persisted QueryMT service endpoint.
pub const SERVICE_ENDPOINT_SECRET_KEY: &str = "service.endpoint";
/// SecretStore key for the persisted QueryMT service OAuth token set.
pub const SERVICE_OAUTH_TOKENS_SECRET_KEY: &str = "service.oauth.tokens";

/// Return the public OAuth client id for this QueryMT build.
pub fn service_oauth_client_id() -> String {
    format_service_oauth_client_id(env!("CARGO_PKG_VERSION"))
}

/// Format a QueryMT service OAuth client id from a package/build version.
pub fn format_service_oauth_client_id(version: &str) -> String {
    format!("querymt-v{version}")
}

/// Return the default loopback redirect URI for QueryMT service OAuth.
pub fn default_service_oauth_redirect_uri() -> Url {
    Url::parse(SERVICE_OAUTH_REDIRECT_URI).expect("static service OAuth redirect URI is valid")
}

/// Result type used by service-client operations.
pub type Result<T> = std::result::Result<T, ServiceClientError>;

/// Errors returned by service-client primitives.
#[derive(Debug)]
pub enum ServiceClientError {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidEndpoint(url::ParseError),
    InvalidOAuthUrl(url::ParseError),
    OAuthTokenExchange(String),
    OAuthTokenRefresh(String),
    OAuthTokenRevoke(String),
    ServiceAuthRequired,
    ServiceTokenRefreshUnavailable,
    ServiceApiTransport(String),
    ServiceApiUnauthorized,
    ServiceApiForbidden,
    ServiceApiHttp {
        status: u16,
        body: String,
    },
    OAuthCallbackTimeout,
    OAuthCallbackInvalidRequest(String),
    OAuthCallbackInvalidPath(String),
    OAuthCallbackMissingCode,
    OAuthCallbackMissingState,
    OAuthCallbackStateMismatch,
    OAuthCallbackUnverifiableManualCode,
    OAuthCallbackError {
        error: String,
        description: Option<String>,
    },
}

impl std::fmt::Display for ServiceClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "service secret store error: {error}"),
            Self::Json(error) => write!(f, "service token serialization error: {error}"),
            Self::InvalidEndpoint(error) => write!(f, "invalid service endpoint: {error}"),
            Self::InvalidOAuthUrl(error) => write!(f, "invalid service OAuth URL: {error}"),
            Self::OAuthTokenExchange(error) => {
                write!(f, "service OAuth token exchange failed: {error}")
            }
            Self::OAuthTokenRefresh(error) => {
                write!(
                    f,
                    "service OAuth token refresh failed; sign in again: {error}"
                )
            }
            Self::OAuthTokenRevoke(error) => {
                write!(f, "service OAuth token revoke failed: {error}")
            }
            Self::ServiceAuthRequired => write!(f, "service sign-in required"),
            Self::ServiceTokenRefreshUnavailable => write!(
                f,
                "service OAuth token is expired and no refresh token is available; sign in again"
            ),
            Self::ServiceApiTransport(error) => {
                write!(f, "service API request failed: {error}")
            }
            Self::ServiceApiUnauthorized => {
                write!(f, "service API rejected the access token; sign in again")
            }
            Self::ServiceApiForbidden => {
                write!(f, "service API forbids this account or token scope")
            }
            Self::ServiceApiHttp { status, body } => {
                write!(f, "service API request failed with HTTP {status}: {body}")
            }
            Self::OAuthCallbackTimeout => write!(f, "timed out waiting for service OAuth callback"),
            Self::OAuthCallbackInvalidRequest(error) => {
                write!(f, "invalid service OAuth callback request: {error}")
            }
            Self::OAuthCallbackInvalidPath(path) => {
                write!(f, "unexpected service OAuth callback path: {path}")
            }
            Self::OAuthCallbackMissingCode => {
                write!(
                    f,
                    "service OAuth callback did not include an authorization code"
                )
            }
            Self::OAuthCallbackMissingState => {
                write!(f, "service OAuth callback did not include state")
            }
            Self::OAuthCallbackStateMismatch => {
                write!(
                    f,
                    "service OAuth callback state did not match the login request"
                )
            }
            Self::OAuthCallbackUnverifiableManualCode => {
                write!(
                    f,
                    "service OAuth manual callback input must include state; paste the full callback URL or query string"
                )
            }
            Self::OAuthCallbackError { error, description } => {
                if let Some(description) = description {
                    write!(
                        f,
                        "service OAuth authorization failed: {error}: {description}"
                    )
                } else {
                    write!(f, "service OAuth authorization failed: {error}")
                }
            }
        }
    }
}

impl std::error::Error for ServiceClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidEndpoint(error) => Some(error),
            Self::InvalidOAuthUrl(error) => Some(error),
            Self::OAuthTokenExchange(_)
            | Self::OAuthTokenRefresh(_)
            | Self::OAuthTokenRevoke(_)
            | Self::ServiceAuthRequired
            | Self::ServiceTokenRefreshUnavailable
            | Self::ServiceApiTransport(_)
            | Self::ServiceApiUnauthorized
            | Self::ServiceApiForbidden
            | Self::ServiceApiHttp { .. }
            | Self::OAuthCallbackTimeout
            | Self::OAuthCallbackInvalidRequest(_)
            | Self::OAuthCallbackInvalidPath(_)
            | Self::OAuthCallbackMissingCode
            | Self::OAuthCallbackMissingState
            | Self::OAuthCallbackStateMismatch
            | Self::OAuthCallbackUnverifiableManualCode
            | Self::OAuthCallbackError { .. } => None,
        }
    }
}

impl From<io::Error> for ServiceClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ServiceClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Configuration inputs used to resolve the QueryMT service endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEndpointConfig {
    /// Optional value from QUERYMT_SERVICE_ENDPOINT.
    pub env_endpoint: Option<String>,
    /// Compiled fallback used when no environment override is present.
    pub default_endpoint: String,
}

impl Default for ServiceEndpointConfig {
    fn default() -> Self {
        Self {
            env_endpoint: std::env::var(QUERYMT_SERVICE_ENDPOINT_ENV).ok(),
            default_endpoint: DEFAULT_SERVICE_ENDPOINT.to_string(),
        }
    }
}

impl ServiceEndpointConfig {
    pub fn new(env_endpoint: Option<String>, default_endpoint: impl Into<String>) -> Self {
        Self {
            env_endpoint,
            default_endpoint: default_endpoint.into(),
        }
    }
}

/// Resolve the QueryMT service endpoint from environment override or default.
pub fn resolve_service_endpoint() -> Result<Url> {
    resolve_service_endpoint_from_config(ServiceEndpointConfig::default())
}

/// Resolve the QueryMT service endpoint from explicit configuration.
pub fn resolve_service_endpoint_from_config(config: ServiceEndpointConfig) -> Result<Url> {
    let endpoint = config
        .env_endpoint
        .as_deref()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .unwrap_or(&config.default_endpoint);

    normalize_service_endpoint(endpoint)
}

fn normalize_service_endpoint(endpoint: &str) -> Result<Url> {
    let mut endpoint = Url::parse(endpoint).map_err(ServiceClientError::InvalidEndpoint)?;
    if !endpoint.path().ends_with('/') {
        let path = format!("{}/", endpoint.path());
        endpoint.set_path(&path);
    }
    Ok(endpoint)
}

/// Authorization Code + PKCE values needed to open login and later exchange the code.
#[derive(Debug)]
pub struct ServiceAuthorizationFlow {
    pub authorization_url: Url,
    pub state: CsrfToken,
    pub pkce_verifier: PkceCodeVerifier,
    pub redirect_uri: Url,
    pub scopes: Vec<String>,
}

/// Validated service OAuth loopback callback payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAuthorizationCallback {
    pub code: String,
    pub state: String,
}

/// Build a service OAuth Authorization Code + PKCE URL with default scopes and redirect URI.
pub fn build_service_authorization_flow(endpoint: Url) -> Result<ServiceAuthorizationFlow> {
    build_service_authorization_flow_with_scopes(endpoint, DEFAULT_SERVICE_OAUTH_SCOPES)
}

/// Build a service OAuth Authorization Code + PKCE URL with explicit scopes.
///
/// The Boruta paths are centralized constants so they can be adjusted with the
/// service contract. This function does not perform network, keyring, or token exchange work.
pub fn build_service_authorization_flow_with_scopes<I, S>(
    endpoint: Url,
    scopes: I,
) -> Result<ServiceAuthorizationFlow>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    build_service_authorization_flow_with_redirect(
        endpoint,
        default_service_oauth_redirect_uri(),
        scopes,
    )
}

/// Build a service OAuth Authorization Code + PKCE URL with explicit redirect URI and scopes.
pub fn build_service_authorization_flow_with_redirect<I, S>(
    endpoint: Url,
    redirect_uri: Url,
    scopes: I,
) -> Result<ServiceAuthorizationFlow>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let client = service_oauth_client(&endpoint)?
        .set_redirect_uri(RedirectUrl::from_url(redirect_uri.clone()));
    let (pkce_code_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let mut authorization_request = client.authorize_url(CsrfToken::new_random);
    let mut requested_scopes = Vec::new();

    for scope in scopes {
        let scope = scope.as_ref();
        requested_scopes.push(scope.to_string());
        authorization_request = authorization_request.add_scope(Scope::new(scope.to_string()));
    }

    let (authorization_url, state) = authorization_request
        .set_pkce_challenge(pkce_code_challenge)
        .url();

    Ok(ServiceAuthorizationFlow {
        authorization_url,
        state,
        pkce_verifier,
        redirect_uri,
        scopes: requested_scopes,
    })
}

/// Minimal authenticated API client for the QueryMT service.
#[derive(Clone, Debug)]
pub struct ServiceApiClient {
    endpoint: Url,
    http_client: reqwest::Client,
}

/// Flexible wrapper for service status payloads while the schema is still evolving.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub raw: serde_json::Value,
}

impl ServiceApiClient {
    pub fn new(endpoint: Url) -> Result<Self> {
        Ok(Self {
            endpoint,
            http_client: service_api_http_client()?,
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub async fn status(&self, store: &mut impl ServiceAuthStore) -> Result<ServiceStatus> {
        self.authenticated_get_json(SERVICE_API_STATUS_PATH, store)
            .await
            .map(|raw| ServiceStatus { raw })
    }

    pub async fn revoke_tokens(&self, tokens: &ServiceTokenSet) -> Result<()> {
        revoke_service_tokens_with_client(&self.http_client, &self.endpoint, tokens).await
    }

    async fn authenticated_get_json(
        &self,
        path: &str,
        store: &mut impl ServiceAuthStore,
    ) -> Result<serde_json::Value> {
        let access_token = get_valid_service_access_token(&self.endpoint, store).await?;
        let url = service_endpoint_url(&self.endpoint, path)?;
        let response = self
            .http_client
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|error| ServiceClientError::ServiceApiTransport(error.to_string()))?;

        map_service_api_response(response).await
    }
}

fn service_endpoint_url(endpoint: &Url, path: &str) -> Result<Url> {
    endpoint
        .join(path.trim_start_matches('/'))
        .map_err(ServiceClientError::InvalidOAuthUrl)
}

/// Revoke persisted service OAuth tokens without mutating local token storage.
pub async fn revoke_service_tokens(endpoint: &Url, tokens: &ServiceTokenSet) -> Result<()> {
    let http_client = service_api_http_client()?;
    revoke_service_tokens_with_client(&http_client, endpoint, tokens).await
}

async fn revoke_service_tokens_with_client(
    http_client: &reqwest::Client,
    endpoint: &Url,
    tokens: &ServiceTokenSet,
) -> Result<()> {
    let Some((token, token_type_hint)) = service_revoke_token_request(tokens) else {
        return Ok(());
    };
    let url = service_endpoint_url(endpoint, SERVICE_OAUTH_REVOKE_PATH)?;
    let form = [
        ("token", token),
        ("token_type_hint", token_type_hint),
        ("client_id", service_oauth_client_id()),
    ];

    let response = http_client
        .post(url)
        .form(&form)
        .send()
        .await
        .map_err(|error| ServiceClientError::OAuthTokenRevoke(error.to_string()))?;

    map_service_revoke_response(response).await
}

fn service_revoke_token_request(tokens: &ServiceTokenSet) -> Option<(String, String)> {
    tokens
        .refresh_token
        .as_ref()
        .filter(|token| !token.is_empty())
        .map(|token| (token.clone(), "refresh_token".to_string()))
        .or_else(|| {
            (!tokens.access_token.is_empty())
                .then(|| (tokens.access_token.clone(), "access_token".to_string()))
        })
}

async fn map_service_revoke_response(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }

    let body = response
        .text()
        .await
        .map_err(|error| ServiceClientError::OAuthTokenRevoke(error.to_string()))?;
    Err(ServiceClientError::OAuthTokenRevoke(format!(
        "HTTP {}: {}",
        status.as_u16(),
        body
    )))
}

/// Wait for the service OAuth redirect on the default loopback callback address.
pub async fn listen_for_service_authorization_callback(
    flow: &ServiceAuthorizationFlow,
    timeout: Duration,
) -> Result<ServiceAuthorizationCallback> {
    wait_for_service_authorization_callback(flow.state.secret(), timeout).await
}

/// Wait for the service OAuth redirect and validate the callback state.
pub async fn wait_for_service_authorization_callback(
    expected_state: &str,
    timeout: Duration,
) -> Result<ServiceAuthorizationCallback> {
    let address = format!("{SERVICE_OAUTH_REDIRECT_HOST}:{SERVICE_OAUTH_REDIRECT_PORT}");
    let listener = TcpListener::bind(&address)
        .await
        .map_err(ServiceClientError::Io)?;

    match time::timeout(
        timeout,
        accept_service_authorization_callback(listener, expected_state),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ServiceClientError::OAuthCallbackTimeout),
    }
}

async fn accept_service_authorization_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<ServiceAuthorizationCallback> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(ServiceClientError::Io)?;
        let request = read_callback_http_request(&mut stream).await;
        let result = request
            .and_then(|request| parse_service_authorization_callback(&request, expected_state));
        let relevant = !matches!(result, Err(ServiceClientError::OAuthCallbackInvalidPath(_)));
        let _ = write_callback_http_response(&mut stream, &result).await;

        if relevant {
            return result;
        }
    }
}

async fn read_callback_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0; 512];

    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(ServiceClientError::Io)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 8192 {
            return Err(ServiceClientError::OAuthCallbackInvalidRequest(
                "request headers exceed 8192 bytes".to_string(),
            ));
        }
    }

    String::from_utf8(buffer).map_err(|error| {
        ServiceClientError::OAuthCallbackInvalidRequest(format!("request is not UTF-8: {error}"))
    })
}

fn parse_service_authorization_callback(
    request: &str,
    expected_state: &str,
) -> Result<ServiceAuthorizationCallback> {
    let request_line = request.lines().next().ok_or_else(|| {
        ServiceClientError::OAuthCallbackInvalidRequest("missing request line".to_string())
    })?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or_else(|| {
        ServiceClientError::OAuthCallbackInvalidRequest("missing method".to_string())
    })?;
    let target = parts.next().ok_or_else(|| {
        ServiceClientError::OAuthCallbackInvalidRequest("missing request target".to_string())
    })?;

    if method != "GET" {
        return Err(ServiceClientError::OAuthCallbackInvalidRequest(format!(
            "unsupported method {method}"
        )));
    }

    parse_service_authorization_callback_target(target, expected_state)
}

/// Parse a manually pasted service OAuth callback URL or query string.
///
/// Raw authorization-code-only input is rejected because it cannot prove the OAuth state.
pub fn parse_manual_service_authorization_callback(
    input: &str,
    expected_state: &str,
) -> Result<ServiceAuthorizationCallback> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ServiceClientError::OAuthCallbackInvalidRequest(
            "manual callback input is empty".to_string(),
        ));
    }

    if input.contains("://") {
        return parse_service_authorization_callback_target(input, expected_state);
    }

    if input.starts_with('?') || looks_like_query_string(input) {
        return parse_service_authorization_callback_query(
            input.trim_start_matches('?'),
            expected_state,
        );
    }

    Err(ServiceClientError::OAuthCallbackUnverifiableManualCode)
}

fn looks_like_query_string(input: &str) -> bool {
    input.split('&').any(|part| {
        let key = part.split_once('=').map_or(part, |(key, _)| key);
        matches!(key, "code" | "state" | "error" | "error_description")
    })
}

fn parse_service_authorization_callback_target(
    target: &str,
    expected_state: &str,
) -> Result<ServiceAuthorizationCallback> {
    let callback_url = if target.contains("://") {
        Url::parse(target)
    } else {
        Url::parse(&format!("http://{SERVICE_OAUTH_REDIRECT_HOST}{target}"))
    }
    .map_err(|error| ServiceClientError::OAuthCallbackInvalidRequest(error.to_string()))?;

    if callback_url.path() != SERVICE_OAUTH_REDIRECT_PATH {
        return Err(ServiceClientError::OAuthCallbackInvalidPath(
            callback_url.path().to_string(),
        ));
    }

    let query = callback_url.query().ok_or_else(|| {
        ServiceClientError::OAuthCallbackInvalidRequest(
            "callback URL did not include a query string".to_string(),
        )
    })?;

    parse_service_authorization_callback_query(query, expected_state)
}

fn parse_service_authorization_callback_query(
    query: &str,
    expected_state: &str,
) -> Result<ServiceAuthorizationCallback> {
    let query = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();

    if let Some(error) = query.get("error") {
        return Err(ServiceClientError::OAuthCallbackError {
            error: error.clone(),
            description: query.get("error_description").cloned(),
        });
    }

    let state = query
        .get("state")
        .filter(|state| !state.is_empty())
        .ok_or(ServiceClientError::OAuthCallbackMissingState)?;
    if state != expected_state {
        return Err(ServiceClientError::OAuthCallbackStateMismatch);
    }

    let code = query
        .get("code")
        .filter(|code| !code.is_empty())
        .ok_or(ServiceClientError::OAuthCallbackMissingCode)?;

    Ok(ServiceAuthorizationCallback {
        code: code.clone(),
        state: state.clone(),
    })
}

async fn write_callback_http_response(
    stream: &mut TcpStream,
    result: &Result<ServiceAuthorizationCallback>,
) -> io::Result<()> {
    let (status, title, message) = if result.is_ok() {
        (
            "200 OK",
            "QueryMT authorization complete",
            "You can close this window and return to QueryMT.",
        )
    } else {
        (
            "400 Bad Request",
            "QueryMT authorization failed",
            "You can close this window and return to QueryMT to try again.",
        )
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head><body><h1>{title}</h1><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// Exchange a validated service authorization code for OAuth tokens.
pub async fn exchange_service_authorization_code(
    endpoint: &Url,
    flow: &ServiceAuthorizationFlow,
    callback: &ServiceAuthorizationCallback,
) -> Result<ServiceTokenSet> {
    if callback.state.as_str() != flow.state.secret() {
        return Err(ServiceClientError::OAuthCallbackStateMismatch);
    }

    let client = service_oauth_client(endpoint)?
        .set_redirect_uri(RedirectUrl::from_url(flow.redirect_uri.clone()));
    let http_client = service_oauth_http_client(ServiceClientError::OAuthTokenExchange)?;

    let token_response = client
        .exchange_code(AuthorizationCode::new(callback.code.clone()))
        .set_pkce_verifier(PkceCodeVerifier::new(
            flow.pkce_verifier.secret().to_string(),
        ))
        .request_async(&http_client)
        .await
        .map_err(map_request_token_exchange_error)?;

    Ok(service_token_set_from_token_response(
        &token_response,
        now_unix_seconds(),
    ))
}

/// Return persisted service OAuth tokens, refreshing expired access tokens when possible.
pub async fn refresh_service_tokens_if_needed(
    endpoint: &Url,
    store: &mut impl ServiceAuthStore,
) -> Result<ServiceTokenSet> {
    let tokens = store
        .get_tokens()?
        .ok_or(ServiceClientError::ServiceAuthRequired)?;

    if tokens.has_valid_access_token() {
        return Ok(tokens);
    }

    let refresh_token = tokens
        .refresh_token
        .as_ref()
        .filter(|token| !token.is_empty())
        .ok_or(ServiceClientError::ServiceTokenRefreshUnavailable)?;

    let refreshed_tokens =
        refresh_service_tokens(endpoint, refresh_token)
            .await
            .map(|mut refreshed_tokens| {
                preserve_refresh_token_if_omitted(&mut refreshed_tokens, &tokens);
                refreshed_tokens
            })?;

    store.set_tokens(&refreshed_tokens)?;
    Ok(refreshed_tokens)
}

/// Return a valid service access token, refreshing and persisting tokens when needed.
pub async fn get_valid_service_access_token(
    endpoint: &Url,
    store: &mut impl ServiceAuthStore,
) -> Result<String> {
    refresh_service_tokens_if_needed(endpoint, store)
        .await
        .map(|tokens| tokens.access_token)
}

async fn refresh_service_tokens(endpoint: &Url, refresh_token: &str) -> Result<ServiceTokenSet> {
    let client = service_oauth_client(endpoint)?;
    let http_client = service_oauth_http_client(ServiceClientError::OAuthTokenRefresh)?;
    let refresh_token = RefreshToken::new(refresh_token.to_string());

    let token_response = client
        .exchange_refresh_token(&refresh_token)
        .request_async(&http_client)
        .await
        .map_err(map_request_token_refresh_error)?;

    Ok(service_token_set_from_token_response(
        &token_response,
        now_unix_seconds(),
    ))
}

fn service_oauth_client(
    endpoint: &Url,
) -> Result<
    oauth2::basic::BasicClient<
        oauth2::EndpointSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointNotSet,
        oauth2::EndpointSet,
    >,
> {
    let token_url = TokenUrl::from_url(service_endpoint_url(endpoint, SERVICE_OAUTH_TOKEN_PATH)?);
    Ok(BasicClient::new(ClientId::new(service_oauth_client_id()))
        .set_auth_uri(AuthUrl::from_url(service_endpoint_url(
            endpoint,
            SERVICE_OAUTH_AUTHORIZE_PATH,
        )?))
        .set_token_uri(token_url))
}

fn service_oauth_http_client(
    error_variant: fn(String) -> ServiceClientError,
) -> Result<oauth2::reqwest::Client> {
    oauth2::reqwest::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error_variant(error.to_string()))
}

fn service_api_http_client() -> Result<reqwest::Client> {
    reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ServiceClientError::ServiceApiTransport(error.to_string()))
}

async fn map_service_api_response(response: reqwest::Response) -> Result<serde_json::Value> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| ServiceClientError::ServiceApiTransport(error.to_string()))?;

    match status.as_u16() {
        200..=299 => serde_json::from_str(&body).map_err(ServiceClientError::Json),
        401 => Err(ServiceClientError::ServiceApiUnauthorized),
        403 => Err(ServiceClientError::ServiceApiForbidden),
        _ => Err(ServiceClientError::ServiceApiHttp {
            status: status.as_u16(),
            body,
        }),
    }
}

fn service_token_set_from_token_response(
    token_response: &BasicTokenResponse,
    now_unix_seconds: u64,
) -> ServiceTokenSet {
    let expires_at = token_response
        .expires_in()
        .map(|duration| now_unix_seconds.saturating_add(duration.as_secs()));
    let scope = token_response.scopes().map(|scopes| {
        scopes
            .iter()
            .map(|scope| scope.as_ref())
            .collect::<Vec<_>>()
            .join(" ")
    });

    ServiceTokenSet {
        access_token: token_response.access_token().secret().to_string(),
        refresh_token: token_response
            .refresh_token()
            .map(|token| token.secret().to_string()),
        token_type: Some(service_token_type_name(token_response.token_type())),
        scope,
        expires_at,
    }
}

fn service_token_type_name(token_type: &BasicTokenType) -> String {
    match token_type {
        BasicTokenType::Bearer => "bearer".to_string(),
        BasicTokenType::Mac => "mac".to_string(),
        BasicTokenType::Extension(value) => value.clone(),
    }
}

fn preserve_refresh_token_if_omitted(
    tokens: &mut ServiceTokenSet,
    previous_tokens: &ServiceTokenSet,
) {
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = previous_tokens.refresh_token.clone();
    }
}

fn map_request_token_exchange_error(
    error: RequestTokenError<
        oauth2::HttpClientError<oauth2::reqwest::Error>,
        oauth2::basic::BasicErrorResponse,
    >,
) -> ServiceClientError {
    ServiceClientError::OAuthTokenExchange(error.to_string())
}

fn map_request_token_refresh_error(
    error: RequestTokenError<
        oauth2::HttpClientError<oauth2::reqwest::Error>,
        oauth2::basic::BasicErrorResponse,
    >,
) -> ServiceClientError {
    ServiceClientError::OAuthTokenRefresh(error.to_string())
}

/// OAuth tokens issued by the QueryMT service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceTokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub scope: Option<String>,
    /// Unix timestamp in seconds when the access token expires.
    pub expires_at: Option<u64>,
}

impl ServiceTokenSet {
    /// Returns true when the token has an expiry timestamp at or before now.
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= now_unix_seconds())
    }

    /// Returns true when an access token is present and not expired.
    pub fn has_valid_access_token(&self) -> bool {
        !self.access_token.is_empty() && !self.is_expired()
    }
}

/// Authentication state derived from persisted service tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAuthStatus {
    SignedOut,
    AccessTokenValid,
    AccessTokenExpired,
}

/// Storage interface for QueryMT service endpoint and OAuth tokens.
pub trait ServiceAuthStore {
    fn set_endpoint(&mut self, endpoint: &Url) -> Result<()>;
    fn get_endpoint(&self) -> Result<Option<Url>>;
    fn delete_endpoint(&mut self) -> Result<()>;
    fn set_tokens(&mut self, tokens: &ServiceTokenSet) -> Result<()>;
    fn get_tokens(&self) -> Result<Option<ServiceTokenSet>>;
    fn delete_tokens(&mut self) -> Result<()>;

    fn auth_status(&self) -> Result<ServiceAuthStatus> {
        Ok(match self.get_tokens()? {
            Some(tokens) if tokens.has_valid_access_token() => ServiceAuthStatus::AccessTokenValid,
            Some(_) => ServiceAuthStatus::AccessTokenExpired,
            None => ServiceAuthStatus::SignedOut,
        })
    }
}

/// SecretStore-backed persistence for QueryMT service auth data.
pub struct SecretStoreServiceAuthStore {
    store: SecretStore,
}

impl SecretStoreServiceAuthStore {
    pub fn new() -> Result<Self> {
        Ok(Self {
            store: SecretStore::new()?,
        })
    }

    pub fn from_secret_store(store: SecretStore) -> Self {
        Self { store }
    }

    pub fn into_secret_store(self) -> SecretStore {
        self.store
    }
}

impl ServiceAuthStore for SecretStoreServiceAuthStore {
    fn set_endpoint(&mut self, endpoint: &Url) -> Result<()> {
        self.store
            .set(SERVICE_ENDPOINT_SECRET_KEY, endpoint.as_str())?;
        Ok(())
    }

    fn get_endpoint(&self) -> Result<Option<Url>> {
        self.store
            .get(SERVICE_ENDPOINT_SECRET_KEY)
            .map(|endpoint| Url::parse(&endpoint).map_err(ServiceClientError::InvalidEndpoint))
            .transpose()
    }

    fn delete_endpoint(&mut self) -> Result<()> {
        self.store.delete(SERVICE_ENDPOINT_SECRET_KEY)?;
        Ok(())
    }

    fn set_tokens(&mut self, tokens: &ServiceTokenSet) -> Result<()> {
        let tokens = serde_json::to_string(tokens)?;
        self.store.set(SERVICE_OAUTH_TOKENS_SECRET_KEY, tokens)?;
        Ok(())
    }

    fn get_tokens(&self) -> Result<Option<ServiceTokenSet>> {
        self.store
            .get(SERVICE_OAUTH_TOKENS_SECRET_KEY)
            .map(|tokens| serde_json::from_str(&tokens).map_err(ServiceClientError::Json))
            .transpose()
    }

    fn delete_tokens(&mut self) -> Result<()> {
        self.store.delete(SERVICE_OAUTH_TOKENS_SECRET_KEY)?;
        Ok(())
    }
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    #[derive(Default)]
    struct MemoryServiceAuthStore {
        endpoint: Option<Url>,
        secrets: HashMap<&'static str, String>,
    }

    async fn read_test_http_request(stream: &mut TcpStream) -> (String, String) {
        let mut buffer = Vec::with_capacity(1024);
        let mut chunk = [0; 512];
        let headers_end = loop {
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                panic!("connection closed before request headers completed");
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
        };

        let headers = String::from_utf8(buffer[..headers_end].to_vec()).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length: "))
            .and_then(|length| length.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = buffer[headers_end..].to_vec();
        if body.len() < content_length {
            body.resize(content_length, 0);
            stream
                .read_exact(&mut body[buffer.len() - headers_end..])
                .await
                .unwrap();
        }
        body.truncate(content_length);

        (headers, String::from_utf8(body).unwrap())
    }

    impl ServiceAuthStore for MemoryServiceAuthStore {
        fn set_endpoint(&mut self, endpoint: &Url) -> Result<()> {
            self.endpoint = Some(endpoint.clone());
            Ok(())
        }

        fn get_endpoint(&self) -> Result<Option<Url>> {
            Ok(self.endpoint.clone())
        }

        fn delete_endpoint(&mut self) -> Result<()> {
            self.endpoint = None;
            Ok(())
        }

        fn set_tokens(&mut self, tokens: &ServiceTokenSet) -> Result<()> {
            self.secrets.insert(
                SERVICE_OAUTH_TOKENS_SECRET_KEY,
                serde_json::to_string(tokens)?,
            );
            Ok(())
        }

        fn get_tokens(&self) -> Result<Option<ServiceTokenSet>> {
            self.secrets
                .get(SERVICE_OAUTH_TOKENS_SECRET_KEY)
                .map(|tokens| serde_json::from_str(tokens).map_err(ServiceClientError::Json))
                .transpose()
        }

        fn delete_tokens(&mut self) -> Result<()> {
            self.secrets.remove(SERVICE_OAUTH_TOKENS_SECRET_KEY);
            Ok(())
        }
    }

    #[test]
    fn service_secret_keys_are_service_specific() {
        assert_eq!(SERVICE_ENDPOINT_SECRET_KEY, "service.endpoint");
        assert_eq!(SERVICE_OAUTH_TOKENS_SECRET_KEY, "service.oauth.tokens");
        assert_ne!(SERVICE_ENDPOINT_SECRET_KEY, SERVICE_OAUTH_TOKENS_SECRET_KEY);
        assert!(SERVICE_ENDPOINT_SECRET_KEY.starts_with("service."));
        assert!(SERVICE_OAUTH_TOKENS_SECRET_KEY.starts_with("service."));
    }

    #[test]
    fn token_status_tracks_missing_valid_and_expired_tokens() {
        let mut store = MemoryServiceAuthStore::default();
        assert_eq!(store.auth_status().unwrap(), ServiceAuthStatus::SignedOut);

        store
            .set_tokens(&ServiceTokenSet {
                access_token: "access".to_string(),
                refresh_token: Some("refresh".to_string()),
                token_type: Some("Bearer".to_string()),
                scope: Some("profile".to_string()),
                expires_at: Some(now_unix_seconds() + 60),
            })
            .unwrap();
        assert_eq!(
            store.auth_status().unwrap(),
            ServiceAuthStatus::AccessTokenValid
        );

        store
            .set_tokens(&ServiceTokenSet {
                access_token: "access".to_string(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: Some(now_unix_seconds().saturating_sub(1)),
            })
            .unwrap();
        assert_eq!(
            store.auth_status().unwrap(),
            ServiceAuthStatus::AccessTokenExpired
        );
    }

    #[test]
    fn token_json_round_trips_without_provider_oauth_types() {
        let tokens = ServiceTokenSet {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            token_type: Some("Bearer".to_string()),
            scope: Some("service:read".to_string()),
            expires_at: Some(123),
        };

        let json = serde_json::to_string(&tokens).unwrap();
        assert_eq!(
            serde_json::from_str::<ServiceTokenSet>(&json).unwrap(),
            tokens
        );
    }

    #[test]
    fn token_response_conversion_preserves_oauth_fields() {
        let mut token_response = BasicTokenResponse::new(
            oauth2::AccessToken::new("access".to_string()),
            BasicTokenType::Bearer,
            oauth2::EmptyExtraTokenFields {},
        );
        token_response.set_refresh_token(Some(oauth2::RefreshToken::new("refresh".to_string())));
        token_response.set_expires_in(Some(&Duration::from_secs(3600)));
        token_response.set_scopes(Some(vec![
            Scope::new("service:status".to_string()),
            Scope::new("service:profile".to_string()),
        ]));

        let tokens = service_token_set_from_token_response(&token_response, 1_700_000_000);

        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(tokens.token_type.as_deref(), Some("bearer"));
        assert_eq!(
            tokens.scope.as_deref(),
            Some("service:status service:profile")
        );
        assert_eq!(tokens.expires_at, Some(1_700_003_600));
    }

    #[tokio::test]
    async fn exchange_rejects_state_mismatch_before_network() {
        let endpoint = Url::parse("https://service.querymt.example/").unwrap();
        let flow = build_service_authorization_flow(endpoint.clone()).unwrap();
        let callback = ServiceAuthorizationCallback {
            code: "code".to_string(),
            state: "wrong-state".to_string(),
        };

        let error = exchange_service_authorization_code(&endpoint, &flow, &callback)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackStateMismatch
        ));
    }

    #[tokio::test]
    async fn refresh_helper_requires_stored_tokens() {
        let endpoint = Url::parse("https://service.querymt.example/").unwrap();
        let mut store = MemoryServiceAuthStore::default();

        let error = refresh_service_tokens_if_needed(&endpoint, &mut store)
            .await
            .unwrap_err();

        assert!(matches!(error, ServiceClientError::ServiceAuthRequired));
        assert_eq!(store.get_tokens().unwrap(), None);
    }

    #[tokio::test]
    async fn refresh_helper_returns_valid_token_without_store_update() {
        let endpoint = Url::parse("https://service.querymt.example/").unwrap();
        let mut store = MemoryServiceAuthStore::default();
        let tokens = ServiceTokenSet {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(now_unix_seconds() + 60),
        };
        store.set_tokens(&tokens).unwrap();

        let access_token = get_valid_service_access_token(&endpoint, &mut store)
            .await
            .unwrap();

        assert_eq!(access_token, "access");
        assert_eq!(store.get_tokens().unwrap(), Some(tokens));
    }

    #[tokio::test]
    async fn refresh_helper_requires_refresh_token_for_expired_token() {
        let endpoint = Url::parse("https://service.querymt.example/").unwrap();
        let mut store = MemoryServiceAuthStore::default();
        let tokens = ServiceTokenSet {
            access_token: "access".to_string(),
            refresh_token: None,
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(now_unix_seconds().saturating_sub(1)),
        };
        store.set_tokens(&tokens).unwrap();

        let error = refresh_service_tokens_if_needed(&endpoint, &mut store)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::ServiceTokenRefreshUnavailable
        ));
        assert_eq!(store.get_tokens().unwrap(), Some(tokens));
    }

    #[test]
    fn refresh_token_preservation_keeps_previous_token_when_response_omits_one() {
        let previous_tokens = ServiceTokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(1),
        };
        let mut refreshed_tokens = ServiceTokenSet {
            access_token: "new-access".to_string(),
            refresh_token: None,
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(2),
        };

        preserve_refresh_token_if_omitted(&mut refreshed_tokens, &previous_tokens);

        assert_eq!(refreshed_tokens.access_token, "new-access");
        assert_eq!(
            refreshed_tokens.refresh_token.as_deref(),
            Some("old-refresh")
        );
    }

    #[test]
    fn refresh_token_preservation_keeps_replacement_refresh_token() {
        let previous_tokens = ServiceTokenSet {
            access_token: "old-access".to_string(),
            refresh_token: Some("old-refresh".to_string()),
            token_type: Some("bearer".to_string()),
            scope: None,
            expires_at: Some(1),
        };
        let mut refreshed_tokens = ServiceTokenSet {
            access_token: "new-access".to_string(),
            refresh_token: Some("new-refresh".to_string()),
            token_type: Some("bearer".to_string()),
            scope: None,
            expires_at: Some(2),
        };

        preserve_refresh_token_if_omitted(&mut refreshed_tokens, &previous_tokens);

        assert_eq!(
            refreshed_tokens.refresh_token.as_deref(),
            Some("new-refresh")
        );
    }

    #[test]
    fn endpoint_round_trips_in_memory_store() {
        let mut store = MemoryServiceAuthStore::default();
        let endpoint = Url::parse("https://api.query.mt/").unwrap();

        store.set_endpoint(&endpoint).unwrap();
        assert_eq!(store.get_endpoint().unwrap(), Some(endpoint));

        store.delete_endpoint().unwrap();
        assert_eq!(store.get_endpoint().unwrap(), None);
    }

    #[test]
    fn endpoint_resolution_uses_env_override() {
        let endpoint = resolve_service_endpoint_from_config(ServiceEndpointConfig::new(
            Some("https://dev.querymt.example/api".to_string()),
            DEFAULT_SERVICE_ENDPOINT,
        ))
        .unwrap();

        assert_eq!(endpoint.as_str(), "https://dev.querymt.example/api/");
    }

    #[test]
    fn endpoint_resolution_rejects_invalid_env_override() {
        let error = resolve_service_endpoint_from_config(ServiceEndpointConfig::new(
            Some("not a url".to_string()),
            DEFAULT_SERVICE_ENDPOINT,
        ))
        .unwrap_err();

        assert!(matches!(error, ServiceClientError::InvalidEndpoint(_)));
    }

    #[test]
    fn endpoint_resolution_uses_default_without_env_override() {
        let endpoint = resolve_service_endpoint_from_config(ServiceEndpointConfig::new(
            None,
            "https://service.querymt.example/root",
        ))
        .unwrap();

        assert_eq!(endpoint.as_str(), "https://service.querymt.example/root/");
    }

    #[test]
    fn service_api_client_joins_status_path_against_endpoint_root() {
        let endpoint = Url::parse("https://service.querymt.example/root/").unwrap();

        let status_url = service_endpoint_url(&endpoint, SERVICE_API_STATUS_PATH).unwrap();

        assert_eq!(
            status_url.as_str(),
            "https://service.querymt.example/root/api/service/status"
        );
    }

    #[test]
    fn service_revoke_prefers_refresh_token_with_hint() {
        let tokens = ServiceTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(now_unix_seconds() + 60),
        };

        let request = service_revoke_token_request(&tokens).unwrap();

        assert_eq!(
            request,
            ("refresh-token".to_string(), "refresh_token".to_string())
        );
    }

    #[test]
    fn service_revoke_falls_back_to_access_token_with_hint() {
        let tokens = ServiceTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: None,
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(now_unix_seconds() + 60),
        };

        let request = service_revoke_token_request(&tokens).unwrap();

        assert_eq!(
            request,
            ("access-token".to_string(), "access_token".to_string())
        );
    }

    #[tokio::test]
    async fn service_revoke_posts_oauth_revocation_form() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Url::parse(&format!("http://{address}/service/")).unwrap();
        let expected_path = "/service/oauth/revoke".to_string();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (request, body) = read_test_http_request(&mut stream).await;

            assert!(request.starts_with(&format!("POST {expected_path} HTTP/1.1\r\n")));
            assert!(request.contains("\r\ncontent-type: application/x-www-form-urlencoded\r\n"));
            assert!(body.contains("token=refresh-token"));
            assert!(body.contains("token_type_hint=refresh_token"));
            assert!(body.contains(&format!("client_id={}", service_oauth_client_id())));

            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        });

        let tokens = ServiceTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(now_unix_seconds() + 60),
        };

        revoke_service_tokens(&endpoint, &tokens).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn service_revoke_maps_non_success_status() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Url::parse(&format!("http://{address}/")).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_callback_http_request(&mut stream).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: 11\r\n\r\nbad request",
                )
                .await
                .unwrap();
        });

        let tokens = ServiceTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: None,
            token_type: Some("bearer".to_string()),
            scope: Some("service:status".to_string()),
            expires_at: Some(now_unix_seconds() + 60),
        };

        let error = revoke_service_tokens(&endpoint, &tokens).await.unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthTokenRevoke(ref message) if message == "HTTP 400: bad request"
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn service_api_client_status_sends_bearer_token_and_parses_json() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Url::parse(&format!("http://{address}/service/")).unwrap();
        let expected_path = "/service/api/service/status".to_string();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_callback_http_request(&mut stream).await.unwrap();

            assert!(request.starts_with(&format!("GET {expected_path} HTTP/1.1\r\n")));
            assert!(request.contains("\r\nauthorization: Bearer access-token\r\n"));

            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 12\r\n\r\n{\"ok\":true}\n",
                )
                .await
                .unwrap();
        });

        let client = ServiceApiClient::new(endpoint).unwrap();
        let mut store = MemoryServiceAuthStore::default();
        store
            .set_tokens(&ServiceTokenSet {
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                token_type: Some("bearer".to_string()),
                scope: Some("service:status".to_string()),
                expires_at: Some(now_unix_seconds() + 60),
            })
            .unwrap();

        let status = client.status(&mut store).await.unwrap();

        assert_eq!(status.raw, serde_json::json!({"ok": true}));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn service_api_client_status_maps_unauthorized_and_forbidden_errors() {
        for (response_status, expected_error) in [
            (
                "401 Unauthorized",
                ServiceClientError::ServiceApiUnauthorized,
            ),
            ("403 Forbidden", ServiceClientError::ServiceApiForbidden),
        ] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let address = listener.local_addr().unwrap();
            let endpoint = Url::parse(&format!("http://{address}/")).unwrap();

            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let _request = read_callback_http_request(&mut stream).await.unwrap();
                let response = format!("HTTP/1.1 {response_status}\r\ncontent-length: 0\r\n\r\n");
                stream.write_all(response.as_bytes()).await.unwrap();
            });

            let client = ServiceApiClient::new(endpoint).unwrap();
            let mut store = MemoryServiceAuthStore::default();
            store
                .set_tokens(&ServiceTokenSet {
                    access_token: "access-token".to_string(),
                    refresh_token: Some("refresh-token".to_string()),
                    token_type: Some("bearer".to_string()),
                    scope: Some("service:status".to_string()),
                    expires_at: Some(now_unix_seconds() + 60),
                })
                .unwrap();

            let error = client.status(&mut store).await.unwrap_err();

            assert_eq!(error.to_string(), expected_error.to_string());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn service_api_client_status_returns_http_error_with_body() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = Url::parse(&format!("http://{address}/")).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_callback_http_request(&mut stream).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\ncontent-type: text/plain\r\ncontent-length: 4\r\n\r\noops",
                )
                .await
                .unwrap();
        });

        let client = ServiceApiClient::new(endpoint).unwrap();
        let mut store = MemoryServiceAuthStore::default();
        store
            .set_tokens(&ServiceTokenSet {
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                token_type: Some("bearer".to_string()),
                scope: Some("service:status".to_string()),
                expires_at: Some(now_unix_seconds() + 60),
            })
            .unwrap();

        let error = client.status(&mut store).await.unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::ServiceApiHttp { status: 500, ref body } if body == "oops"
        ));
        server.await.unwrap();
    }

    #[test]
    fn authorization_flow_builds_authorize_url_with_default_pkce_values() {
        let endpoint = Url::parse("https://service.querymt.example/api/").unwrap();
        let flow = build_service_authorization_flow(endpoint).unwrap();

        assert_eq!(
            flow.authorization_url.as_str().split('?').next().unwrap(),
            "https://service.querymt.example/api/oauth/authorize"
        );
        assert_eq!(flow.redirect_uri, default_service_oauth_redirect_uri());
        assert_eq!(flow.scopes, vec!["service:status".to_string()]);
        assert!(!flow.state.secret().is_empty());
        assert!(!flow.pkce_verifier.secret().is_empty());

        let query = flow
            .authorization_url
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some(service_oauth_client_id().as_str())
        );
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some(SERVICE_OAUTH_REDIRECT_URI)
        );
        assert_eq!(
            query.get("state").map(String::as_str),
            Some(flow.state.secret().as_str())
        );
        assert!(
            query
                .get("code_challenge")
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("scope").map(String::as_str),
            Some("service:status")
        );
    }

    #[test]
    fn authorization_flow_allows_scope_and_redirect_override() {
        let endpoint = Url::parse("https://service.querymt.example/").unwrap();
        let redirect_uri = Url::parse("http://127.0.0.1:2455/alternate/callback").unwrap();
        let flow = build_service_authorization_flow_with_redirect(
            endpoint,
            redirect_uri.clone(),
            ["service:status", "service:profile"],
        )
        .unwrap();

        assert_eq!(flow.redirect_uri, redirect_uri);
        assert_eq!(
            flow.scopes,
            vec!["service:status".to_string(), "service:profile".to_string()]
        );

        let query = flow
            .authorization_url
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some("http://127.0.0.1:2455/alternate/callback")
        );
        assert_eq!(
            query.get("scope").map(String::as_str),
            Some("service:status service:profile")
        );
    }

    #[test]
    fn authorization_flow_omits_scope_when_empty() {
        let endpoint = Url::parse("https://service.querymt.example/").unwrap();
        let flow = build_service_authorization_flow_with_scopes(endpoint, [] as [&str; 0]).unwrap();

        let query = flow
            .authorization_url
            .query_pairs()
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert!(!query.contains_key("scope"));
    }

    #[test]
    fn callback_parser_returns_code_for_matching_state() {
        let request = "GET /auth/callback?code=abc123&state=expected HTTP/1.1\r\nHost: 127.0.0.1:1455\r\n\r\n";

        let callback = parse_service_authorization_callback(request, "expected").unwrap();

        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "expected");
    }

    #[test]
    fn callback_parser_decodes_query_parameters() {
        let request = "GET /auth/callback?code=abc%20123&state=expected%2Fstate HTTP/1.1\r\nHost: 127.0.0.1:1455\r\n\r\n";

        let callback = parse_service_authorization_callback(request, "expected/state").unwrap();

        assert_eq!(callback.code, "abc 123");
        assert_eq!(callback.state, "expected/state");
    }

    #[test]
    fn callback_parser_rejects_unexpected_path() {
        let request =
            "GET /wrong?code=abc123&state=expected HTTP/1.1\r\nHost: 127.0.0.1:1455\r\n\r\n";

        let error = parse_service_authorization_callback(request, "expected").unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackInvalidPath(ref path) if path == "/wrong"
        ));
    }

    #[test]
    fn callback_parser_rejects_state_mismatch() {
        let request =
            "GET /auth/callback?code=abc123&state=wrong HTTP/1.1\r\nHost: 127.0.0.1:1455\r\n\r\n";

        let error = parse_service_authorization_callback(request, "expected").unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackStateMismatch
        ));
    }

    #[test]
    fn callback_parser_requires_code() {
        let request = "GET /auth/callback?state=expected HTTP/1.1\r\nHost: 127.0.0.1:1455\r\n\r\n";

        let error = parse_service_authorization_callback(request, "expected").unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackMissingCode
        ));
    }

    #[test]
    fn callback_parser_returns_oauth_error() {
        let request = "GET /auth/callback?error=access_denied&error_description=Nope HTTP/1.1\r\nHost: 127.0.0.1:1455\r\n\r\n";

        let error = parse_service_authorization_callback(request, "expected").unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackError { ref error, ref description }
                if error == "access_denied" && description.as_deref() == Some("Nope")
        ));
    }

    #[test]
    fn manual_callback_parser_accepts_full_callback_url() {
        let input = "http://127.0.0.1:1455/auth/callback?code=abc123&state=expected";

        let callback = parse_manual_service_authorization_callback(input, "expected").unwrap();

        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "expected");
    }

    #[test]
    fn manual_callback_parser_accepts_bare_query_string() {
        let input = "code=abc%20123&state=expected%2Fstate";

        let callback =
            parse_manual_service_authorization_callback(input, "expected/state").unwrap();

        assert_eq!(callback.code, "abc 123");
        assert_eq!(callback.state, "expected/state");
    }

    #[test]
    fn manual_callback_parser_accepts_query_string_with_leading_question_mark() {
        let input = "?code=abc123&state=expected";

        let callback = parse_manual_service_authorization_callback(input, "expected").unwrap();

        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "expected");
    }

    #[test]
    fn manual_callback_parser_rejects_state_mismatch() {
        let input = "http://127.0.0.1:1455/auth/callback?code=abc123&state=wrong";

        let error = parse_manual_service_authorization_callback(input, "expected").unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackStateMismatch
        ));
    }

    #[test]
    fn manual_callback_parser_returns_oauth_error() {
        let input = "error=access_denied&error_description=Nope&state=expected";

        let error = parse_manual_service_authorization_callback(input, "expected").unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackError { ref error, ref description }
                if error == "access_denied" && description.as_deref() == Some("Nope")
        ));
    }

    #[test]
    fn manual_callback_parser_rejects_raw_code() {
        let input = "abc123";

        let error = parse_manual_service_authorization_callback(input, "expected").unwrap_err();

        assert!(matches!(
            error,
            ServiceClientError::OAuthCallbackUnverifiableManualCode
        ));
    }

    #[tokio::test]
    async fn callback_listener_times_out() {
        let result =
            wait_for_service_authorization_callback("expected", Duration::from_millis(1)).await;

        match result {
            Err(ServiceClientError::OAuthCallbackTimeout) => {}
            Err(ServiceClientError::Io(error)) if error.kind() == io::ErrorKind::AddrInUse => {}
            other => panic!("unexpected callback listener result: {other:?}"),
        }
    }

    #[test]
    fn oauth_client_id_formats_version() {
        assert_eq!(format_service_oauth_client_id("1.2.3"), "querymt-v1.2.3");
    }

    #[test]
    fn oauth_client_id_uses_package_version() {
        assert_eq!(
            service_oauth_client_id(),
            format!("querymt-v{}", env!("CARGO_PKG_VERSION"))
        );
    }
}
