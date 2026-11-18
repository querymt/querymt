//! Built-in OAuth authorization server for ChatGPT-style MCP clients.
//!
//! Dynamic client registration, authorization code + PKCE (S256), and signed,
//! stateless client IDs and tokens. The signing key and owner approval password
//! live in the QueryMT secret store; only pending authorization codes are kept
//! in memory.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Form, Json, Router,
    extract::{Query, Request, State},
    http::{
        StatusCode,
        header::{AUTHORIZATION, CACHE_CONTROL, LOCATION, WWW_AUTHENTICATE},
    },
    middleware::Next,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use hmac::{Hmac, Mac};
use querymt_utils::secret_store::SecretStore;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{oauth::https_url, transport::bearer_credential};

const SCOPE: &str = "qmcp:access";
/// Secret store keys, shared with `qmt secrets get|set|delete`.
pub const SIGNING_KEY: &str = "qmcp.oauth.signing_key";
pub const PASSWORD_KEY: &str = "qmcp.oauth.password";
const CODE_TTL: u64 = 5 * 60;
const ACCESS_TTL: u64 = 60 * 60;
const REFRESH_TTL: u64 = 30 * 24 * 60 * 60;

/// Signing key and owner approval password. Debug never reveals them.
#[derive(Clone)]
pub struct OAuthKeys {
    signing_key: Vec<u8>,
    password: String,
}

impl std::fmt::Debug for OAuthKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OAuthKeys([redacted])")
    }
}

impl OAuthKeys {
    pub fn new(signing_key: Vec<u8>, password: String) -> Result<Self> {
        if signing_key.len() < 32 || password.is_empty() {
            bail!("OAuth signing key must be at least 32 bytes and the password nonempty");
        }
        Ok(Self {
            signing_key,
            password,
        })
    }

    /// Load the keys from the QueryMT secret store, creating missing ones.
    /// A newly created approval password is printed once to stderr.
    pub async fn load_or_create() -> Result<Self> {
        let store = SecretStore::new_async()
            .await
            .context("open the QueryMT secret store")?;
        let (signing_key, password, created) = store
            .run(|store| {
                let signing_key = match store.try_get(SIGNING_KEY)? {
                    Some(value) => value,
                    None => {
                        let value = B64.encode(random::<32>());
                        store.set(SIGNING_KEY, value.clone())?;
                        value
                    }
                };
                let (password, created) = match store.try_get(PASSWORD_KEY)? {
                    Some(value) => (value, false),
                    None => {
                        let value = B64.encode(random::<18>());
                        store.set(PASSWORD_KEY, value.clone())?;
                        (value, true)
                    }
                };
                Ok((signing_key, password, created))
            })
            .await
            .context("read OAuth keys from the QueryMT secret store")?;
        if created {
            eprintln!(
                "qmcp: created OAuth approval password {password}\n\
                 qmcp: it is stored as {PASSWORD_KEY}; show it again with `qmt secrets get {PASSWORD_KEY}`"
            );
        }
        let signing_key = B64
            .decode(signing_key.trim())
            .with_context(|| format!("{SIGNING_KEY} is invalid; delete it to regenerate"))?;
        Self::new(signing_key, password)
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).expect("system randomness is available");
    bytes
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Signed payloads. The tag keeps client IDs, access and refresh tokens apart.
#[derive(Serialize, Deserialize)]
#[serde(tag = "typ", rename_all = "lowercase")]
enum Claims {
    Client {
        redirect_uris: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Access {
        aud: String,
        exp: u64,
    },
    Refresh {
        aud: String,
        client: String,
        exp: u64,
    },
}

struct PendingCode {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    exp: u64,
}

/// Built-in authorization server and token check for one public /mcp resource.
#[derive(Clone)]
pub struct LocalOAuth(Arc<Inner>);

struct Inner {
    resource: String,
    issuer: String,
    host: String,
    metadata_url: String,
    keys: OAuthKeys,
    codes: Mutex<HashMap<String, PendingCode>>,
}

impl LocalOAuth {
    /// `resource` is the public HTTPS /mcp URL; its origin becomes the issuer.
    pub fn new(resource: &str, keys: OAuthKeys) -> Result<Self> {
        let url = https_url(resource)?;
        if url.path() != "/mcp" {
            bail!("OAuth resource must be the public HTTPS /mcp URL");
        }
        let host = url.host_str().context("OAuth resource needs a host")?;
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let issuer = url.origin().ascii_serialization();
        Ok(Self(Arc::new(Inner {
            metadata_url: format!("{issuer}/.well-known/oauth-protected-resource/mcp"),
            resource: url.to_string(),
            issuer,
            host,
            keys,
            codes: Mutex::default(),
        })))
    }

    /// Public host[:port] that must be accepted by the HTTP host check.
    pub(crate) fn public_host(&self) -> &str {
        &self.0.host
    }

    /// Discovery, registration, authorization and token endpoints.
    pub(crate) fn routes(&self) -> Router {
        Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(server_metadata),
            )
            .route("/register", post(register))
            .route("/authorize", get(authorize_page).post(authorize_submit))
            .route("/token", post(token))
            .with_state(self.clone())
    }

    fn sign(&self, claims: &Claims) -> String {
        let payload = B64.encode(serde_json::to_vec(claims).expect("claims serialize"));
        let signature = B64.encode(self.mac(&payload).finalize().into_bytes());
        format!("{payload}.{signature}")
    }

    fn verify(&self, token: &str) -> Option<Claims> {
        let (payload, signature) = token.split_once('.')?;
        self.mac(payload)
            .verify_slice(&B64.decode(signature).ok()?)
            .ok()?;
        serde_json::from_slice(&B64.decode(payload).ok()?).ok()
    }

    fn mac(&self, payload: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0.keys.signing_key)
            .expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        mac
    }

    fn accepts(&self, token: &str) -> bool {
        matches!(
            self.verify(token),
            Some(Claims::Access { aud, exp }) if aud == self.0.resource && exp > now()
        )
    }

    fn check(&self, params: &AuthorizeParams) -> Result<Approved, Box<Response>> {
        let client_id = params.client_id.as_deref().unwrap_or_default();
        let Some(Claims::Client {
            redirect_uris,
            name,
        }) = self.verify(client_id)
        else {
            return Err(Box::new(error_page("Unknown OAuth client.")));
        };
        let redirect_uri = match (&params.redirect_uri, redirect_uris.as_slice()) {
            (Some(uri), uris) if uris.contains(uri) => uri.clone(),
            (None, [only]) => only.clone(),
            _ => {
                return Err(Box::new(error_page(
                    "The redirect URI is not registered for this client.",
                )));
            }
        };
        let fail = |error: &str| -> Result<Approved, Box<Response>> {
            Err(Box::new(self.redirect(
                &redirect_uri,
                error,
                params.state.as_deref(),
            )))
        };
        if params.response_type.as_deref() != Some("code") {
            return fail("unsupported_response_type");
        }
        let Some(challenge) = params
            .code_challenge
            .clone()
            .filter(|_| params.code_challenge_method.as_deref() == Some("S256"))
        else {
            return fail("invalid_request");
        };
        if params
            .resource
            .as_deref()
            .is_some_and(|resource| resource != self.0.resource)
        {
            return fail("invalid_target");
        }
        Ok(Approved {
            client_id: client_id.to_owned(),
            name,
            redirect_uri,
            challenge,
        })
    }

    /// Redirect back to the client with an OAuth `error`.
    fn redirect(&self, uri: &str, error: &str, state: Option<&str>) -> Response {
        self.redirect_with(uri, ("error", error), state)
    }

    /// Redirect back to the client with one result parameter, `state` and `iss`.
    fn redirect_with(
        &self,
        uri: &str,
        (key, value): (&str, &str),
        state: Option<&str>,
    ) -> Response {
        let mut url = Url::parse(uri).expect("registered redirect URIs are valid URLs");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair(key, value);
            if let Some(state) = state {
                query.append_pair("state", state);
            }
            query.append_pair("iss", &self.0.issuer);
        }
        (StatusCode::SEE_OTHER, [(LOCATION, url.to_string())]).into_response()
    }
}

/// Reject /mcp requests without a valid access token for this resource.
pub(crate) async fn require_token(
    State(auth): State<LocalOAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    let accepted = bearer_credential(request.headers()).map(|token| auth.accepts(token));
    if accepted == Some(true) {
        request.headers_mut().remove(AUTHORIZATION);
        return next.run(request).await;
    }
    let mut challenge = format!(
        "Bearer resource_metadata=\"{}\", scope=\"{SCOPE}\"",
        auth.0.metadata_url
    );
    if accepted.is_some() {
        challenge.push_str(", error=\"invalid_token\"");
    }
    (
        StatusCode::UNAUTHORIZED,
        [(WWW_AUTHENTICATE, challenge)],
        [(CACHE_CONTROL, "no-store")],
        "authentication required",
    )
        .into_response()
}

fn no_store(response: impl IntoResponse) -> Response {
    ([(CACHE_CONTROL, "no-store")], response).into_response()
}

fn oauth_error(error: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(CACHE_CONTROL, "no-store")],
        Json(json!({ "error": error })),
    )
        .into_response()
}

async fn resource_metadata(State(auth): State<LocalOAuth>) -> Response {
    no_store(Json(json!({
        "resource": auth.0.resource,
        "authorization_servers": [auth.0.issuer],
        "scopes_supported": [SCOPE],
        "bearer_methods_supported": ["header"],
    })))
}

async fn server_metadata(State(auth): State<LocalOAuth>) -> Response {
    let issuer = &auth.0.issuer;
    no_store(Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "registration_endpoint": format!("{issuer}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE],
        "authorization_response_iss_parameter_supported": true,
    })))
}

#[derive(Deserialize)]
struct Registration {
    #[serde(default)]
    redirect_uris: Vec<String>,
    client_name: Option<String>,
}

fn valid_redirect(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.fragment().is_none()
            && (url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))))
    })
}

async fn register(State(auth): State<LocalOAuth>, Json(request): Json<Registration>) -> Response {
    if request.redirect_uris.is_empty() || !request.redirect_uris.iter().all(|u| valid_redirect(u))
    {
        return oauth_error("invalid_redirect_uri");
    }
    let client_id = auth.sign(&Claims::Client {
        redirect_uris: request.redirect_uris.clone(),
        name: request.client_name.clone(),
    });
    let mut client = json!({
        "client_id": client_id,
        "client_id_issued_at": now(),
        "redirect_uris": request.redirect_uris,
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
    });
    if let Some(name) = request.client_name {
        client["client_name"] = json!(name);
    }
    (StatusCode::CREATED, no_store(Json(client))).into_response()
}

#[derive(Deserialize)]
struct AuthorizeParams {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    resource: Option<String>,
    password: Option<String>,
    decision: Option<String>,
}

struct Approved {
    client_id: String,
    name: Option<String>,
    redirect_uri: String,
    challenge: String,
}

async fn authorize_page(
    State(auth): State<LocalOAuth>,
    Query(params): Query<AuthorizeParams>,
) -> Response {
    match auth.check(&params) {
        Ok(approved) => consent_page(&params, &approved, None),
        Err(response) => *response,
    }
}

async fn authorize_submit(
    State(auth): State<LocalOAuth>,
    Form(params): Form<AuthorizeParams>,
) -> Response {
    let approved = match auth.check(&params) {
        Ok(approved) => approved,
        Err(response) => return *response,
    };
    let state = params.state.as_deref();
    if params.decision.as_deref() != Some("approve") {
        return auth.redirect(&approved.redirect_uri, "access_denied", state);
    }
    let password = params.password.as_deref().unwrap_or_default();
    if !bool::from(password.as_bytes().ct_eq(auth.0.keys.password.as_bytes())) {
        return consent_page(&params, &approved, Some("Wrong approval password."));
    }
    let code = B64.encode(random::<32>());
    {
        let mut codes = auth.0.codes.lock().unwrap_or_else(PoisonError::into_inner);
        let now = now();
        codes.retain(|_, pending| pending.exp > now);
        codes.insert(
            code.clone(),
            PendingCode {
                client_id: approved.client_id,
                redirect_uri: approved.redirect_uri.clone(),
                challenge: approved.challenge,
                exp: now + CODE_TTL,
            },
        );
    }
    auth.redirect_with(&approved.redirect_uri, ("code", &code), state)
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn error_page(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(CACHE_CONTROL, "no-store")],
        Html(format!(
            "<!doctype html><meta charset=\"utf-8\"><title>qmcp</title><p>{}</p>",
            escape(message)
        )),
    )
        .into_response()
}

fn consent_page(params: &AuthorizeParams, approved: &Approved, error: Option<&str>) -> Response {
    let hidden: String = [
        ("response_type", params.response_type.as_deref()),
        ("client_id", Some(approved.client_id.as_str())),
        ("redirect_uri", Some(approved.redirect_uri.as_str())),
        ("code_challenge", params.code_challenge.as_deref()),
        (
            "code_challenge_method",
            params.code_challenge_method.as_deref(),
        ),
        ("state", params.state.as_deref()),
        ("resource", params.resource.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, value)| {
        value.map(|value| {
            format!(
                "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                escape(value)
            )
        })
    })
    .collect();
    let client = escape(approved.name.as_deref().unwrap_or("An MCP client"));
    let redirect = escape(&approved.redirect_uri);
    let error = error
        .map(|error| format!("<p style=\"color:#b00\">{}</p>", escape(error)))
        .unwrap_or_default();
    let status = if error.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::UNAUTHORIZED
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>Authorize qmcp</title>\
         <body style=\"font-family:sans-serif;max-width:30rem;margin:3rem auto;padding:0 1rem\">\
         <h1>Authorize qmcp</h1>\
         <p><b>{client}</b> wants to control your QueryMT agent and will return to <code>{redirect}</code>.</p>\
         {error}<form method=\"post\" action=\"/authorize\">{hidden}\
         <p><label>Approval password <input type=\"password\" name=\"password\" autofocus></label></p>\
         <p><button name=\"decision\" value=\"approve\">Approve</button> \
         <button name=\"decision\" value=\"deny\">Deny</button></p></form>"
    );
    (status, [(CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

#[derive(Deserialize)]
struct TokenParams {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

async fn token(State(auth): State<LocalOAuth>, Form(params): Form<TokenParams>) -> Response {
    let resource = &auth.0.resource;
    if params.resource.as_ref().is_some_and(|r| r != resource) {
        return oauth_error("invalid_target");
    }
    let client_id = match params.grant_type.as_deref() {
        Some("authorization_code") => {
            let pending = auth
                .0
                .codes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(params.code.as_deref().unwrap_or_default());
            let valid = |pending: &PendingCode| {
                pending.exp > now()
                    && params.client_id.as_ref() == Some(&pending.client_id)
                    && params.redirect_uri.as_ref() == Some(&pending.redirect_uri)
                    && params.code_verifier.as_deref().is_some_and(|verifier| {
                        B64.encode(Sha256::digest(verifier.as_bytes())) == pending.challenge
                    })
            };
            match pending.filter(valid) {
                Some(pending) => pending.client_id,
                None => return oauth_error("invalid_grant"),
            }
        }
        Some("refresh_token") => {
            match auth.verify(params.refresh_token.as_deref().unwrap_or_default()) {
                Some(Claims::Refresh { aud, client, exp })
                    if aud == *resource
                        && exp > now()
                        && params.client_id.as_ref().is_none_or(|id| *id == client) =>
                {
                    client
                }
                _ => return oauth_error("invalid_grant"),
            }
        }
        _ => return oauth_error("unsupported_grant_type"),
    };
    let now = now();
    let access_token = auth.sign(&Claims::Access {
        aud: resource.clone(),
        exp: now + ACCESS_TTL,
    });
    let refresh_token = auth.sign(&Claims::Refresh {
        aud: resource.clone(),
        client: client_id,
        exp: now + REFRESH_TTL,
    });
    no_store(Json(json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": ACCESS_TTL,
        "refresh_token": refresh_token,
        "scope": SCOPE,
    })))
}

#[cfg(test)]
mod tests;
