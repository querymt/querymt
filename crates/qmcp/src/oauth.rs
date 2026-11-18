//! Minimal OAuth resource server: discovery plus uncached RFC 7662 introspection.
//! Login, PKCE, client registration and token issuance belong to the external provider.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json,
    extract::{Request, State},
    http::{
        HeaderValue, StatusCode,
        header::{AUTHORIZATION, CACHE_CONTROL, WWW_AUTHENTICATE},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::transport::bearer_credential;

const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Provider settings loaded by --oauth-config. Client secrets stay in the environment.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    pub resource: String,
    pub issuer: String,
    pub introspection_url: String,
    pub client_id: String,
    #[serde(default = "default_scope")]
    pub scope: String,
}

fn default_scope() -> String {
    "qmcp:access".into()
}

/// Validated OAuth mode; deliberately does not expose credentials through Debug.
#[derive(Clone)]
pub struct OAuthAuth(Arc<Provider>);

struct Provider {
    config: OAuthConfig,
    secret: String,
    client: Client,
    endpoint: Url,
    metadata_url: String,
}

#[derive(Debug, PartialEq)]
enum AuthError {
    Invalid,
    Scope,
    Unavailable,
}

impl OAuthAuth {
    /// Require public HTTPS identifiers and a confidential introspection client.
    pub fn new(config: OAuthConfig, client_secret: String) -> Result<Self> {
        let resource = https_url(&config.resource)?;
        https_url(&config.issuer)?;
        let endpoint = https_url(&config.introspection_url)?;
        if resource.path() != "/mcp" {
            bail!("OAuth resource must be the public HTTPS /mcp URL");
        }
        if config.client_id.trim().is_empty() || client_secret.trim().is_empty() {
            bail!("OAuth introspection client ID and secret must not be empty");
        }
        if config.scope.is_empty()
            || !config
                .scope
                .bytes()
                .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
        {
            bail!("OAuth scope must be a single nonempty ASCII scope without quotes or whitespace");
        }
        let metadata_url = resource
            .join("/.well-known/oauth-protected-resource/mcp")?
            .to_string();
        let client = Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
            .context("build OAuth introspection client")?;
        Ok(Self(Arc::new(Provider {
            config,
            secret: client_secret,
            client,
            endpoint,
            metadata_url,
        })))
    }

    pub(crate) fn metadata(&self) -> Value {
        json!({
            "resource": self.0.config.resource,
            "authorization_servers": [self.0.config.issuer],
            "scopes_supported": [self.0.config.scope],
            "bearer_methods_supported": ["header"],
        })
    }

    fn challenge(&self, error: Option<&str>) -> HeaderValue {
        let mut value = format!(
            "Bearer resource_metadata=\"{}\", scope=\"{}\"",
            self.0.metadata_url, self.0.config.scope
        );
        if let Some(error) = error {
            value.push_str(&format!(", error=\"{error}\""));
        }
        HeaderValue::from_str(&value)
            .expect("validated OAuth URLs and scope are safe header values")
    }

    async fn verify(&self, token: &str) -> Result<(), AuthError> {
        let provider = &self.0;
        // Only the configured HTTPS provider receives the token, in a POST body.
        // Never follow redirects, cache credentials, or expose provider errors/bodies.
        let mut response = provider
            .client
            .post(provider.endpoint.clone())
            .header("accept", "application/json")
            .form(&[
                ("token", token),
                ("token_type_hint", "access_token"),
                ("client_id", provider.config.client_id.as_str()),
                ("client_secret", provider.secret.as_str()),
            ])
            .send()
            .await
            .map_err(|_| AuthError::Unavailable)?;
        if !response.status().is_success() {
            return Err(AuthError::Unavailable);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Unavailable)? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(AuthError::Unavailable);
            }
            body.extend_from_slice(&chunk);
        }
        let claims: Value = serde_json::from_slice(&body).map_err(|_| AuthError::Unavailable)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthError::Unavailable)?
            .as_secs();
        validate_claims(&claims, &provider.config, now)
    }
}

pub(crate) fn https_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)
        .map_err(|_| anyhow::anyhow!("OAuth identifiers and endpoints must be HTTPS URLs"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "OAuth identifiers and endpoints must be HTTPS URLs without credentials, queries or fragments"
        );
    }
    Ok(url)
}

fn validate_claims(claims: &Value, config: &OAuthConfig, now: u64) -> Result<(), AuthError> {
    let audience_matches = match &claims["aud"] {
        Value::String(aud) => aud == &config.resource,
        Value::Array(aud) => {
            aud.iter().all(Value::is_string)
                && aud.iter().any(|v| v.as_str() == Some(&config.resource))
        }
        _ => false,
    };
    if claims["active"] != true
        || claims["iss"].as_str() != Some(&config.issuer)
        || !audience_matches
        || !claims["exp"].as_u64().is_some_and(|exp| exp > now)
        || claims
            .get("nbf")
            .is_some_and(|nbf| !nbf.as_u64().is_some_and(|nbf| nbf <= now))
        || !claims["token_type"]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case("Bearer"))
    {
        return Err(AuthError::Invalid);
    }
    if !claims["scope"]
        .as_str()
        .is_some_and(|scopes| scopes.split(' ').any(|s| s == config.scope))
    {
        return Err(AuthError::Scope);
    }
    Ok(())
}

pub(crate) async fn require_oauth(
    State(auth): State<OAuthAuth>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_credential(request.headers()) else {
        return (
            StatusCode::UNAUTHORIZED,
            [
                (WWW_AUTHENTICATE, auth.challenge(None)),
                (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            "authentication required",
        )
            .into_response();
    };
    match auth.verify(token).await {
        Ok(()) => {
            request.headers_mut().remove(AUTHORIZATION);
            next.run(request).await
        }
        Err(AuthError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(CACHE_CONTROL, "no-store")],
            "authorization service unavailable",
        )
            .into_response(),
        Err(error) => {
            let (status, code) = if error == AuthError::Scope {
                (StatusCode::FORBIDDEN, "insufficient_scope")
            } else {
                (StatusCode::UNAUTHORIZED, "invalid_token")
            };
            (
                status,
                [
                    (WWW_AUTHENTICATE, auth.challenge(Some(code))),
                    (CACHE_CONTROL, HeaderValue::from_static("no-store")),
                ],
                "access denied",
            )
                .into_response()
        }
    }
}

pub(crate) async fn metadata(State(auth): State<OAuthAuth>) -> impl IntoResponse {
    ([(CACHE_CONTROL, "no-store")], Json(auth.metadata()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BridgeConfig, QmcpServer, policy::PermissionPolicy, transport};
    use axum::{Form, Router, routing::post};
    use std::{
        collections::HashMap,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn config() -> OAuthConfig {
        OAuthConfig {
            resource: "https://mcp.example.com/mcp".into(),
            issuer: "https://auth.example.com/".into(),
            introspection_url: "https://auth.example.com/introspect".into(),
            client_id: "qmcp-resource-server".into(),
            scope: default_scope(),
        }
    }

    fn claims() -> Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        json!({"active": true, "iss": config().issuer, "aud": config().resource,
            "exp": now + 3600, "scope": "qmcp:access", "token_type": "Bearer"})
    }

    #[test]
    fn config_requires_https_and_header_safe_public_identifiers() {
        for bad in [
            "http://mcp.example.com/mcp",
            "https://user:secret@mcp.example.com/mcp",
            "https://mcp.example.com/mcp?q=1",
            "https://mcp.example.com/mcp#part",
            "not-a-url",
        ] {
            let mut c = config();
            c.resource = bad.into();
            assert!(OAuthAuth::new(c, "secret".into()).is_err());
        }
        for bad in ["", "two scopes", "quote\"", "back\\slash", "line\nbreak"] {
            let mut c = config();
            c.scope = bad.into();
            assert!(OAuthAuth::new(c, "secret".into()).is_err());
        }
        let mut c = config();
        c.introspection_url = "http://127.0.0.1/introspect".into();
        assert!(OAuthAuth::new(c, "secret".into()).is_err());
        assert!(OAuthAuth::new(config(), String::new()).is_err());
        let mut c = config();
        c.client_id.clear();
        assert!(OAuthAuth::new(c, "secret".into()).is_err());
    }

    #[test]
    fn token_claims_require_active_issuer_audience_expiry_and_scope() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(validate_claims(&claims(), &config(), now), Ok(()));
        for field in ["active", "iss", "aud", "exp", "token_type"] {
            let mut value = claims();
            value.as_object_mut().unwrap().remove(field);
            assert_eq!(
                validate_claims(&value, &config(), now),
                Err(AuthError::Invalid),
                "missing {field}"
            );
        }
        for (field, value) in [
            ("active", json!(false)),
            ("active", json!("true")),
            ("iss", json!("https://other.example.com/")),
            ("aud", json!("https://other.example.com/mcp")),
            ("exp", json!(now)),
            ("exp", json!("never")),
            ("nbf", json!(now + 1)),
            ("nbf", Value::Null),
            ("token_type", json!("refresh_token")),
        ] {
            let mut c = claims();
            c[field] = value;
            assert_eq!(
                validate_claims(&c, &config(), now),
                Err(AuthError::Invalid),
                "invalid {field}"
            );
        }
        for scope in [
            Value::Null,
            json!(""),
            json!("qmcp:access-extra"),
            json!(["qmcp:access"]),
        ] {
            let mut c = claims();
            c["scope"] = scope;
            assert_eq!(validate_claims(&c, &config(), now), Err(AuthError::Scope));
        }
        let mut c = claims();
        c["aud"] = json!(["other", config().resource]);
        c["nbf"] = json!(now);
        c["scope"] = json!("other qmcp:access");
        assert_eq!(validate_claims(&c, &config(), now), Ok(()));
    }

    #[tokio::test]
    async fn discovery_introspection_and_revocation_work_without_token_caching() {
        let current = Arc::new(Mutex::new(claims()));
        let calls = Arc::new(AtomicUsize::new(0));
        let current_reply = current.clone();
        let recorded = calls.clone();
        let redirects = Arc::new(AtomicUsize::new(0));
        let leaked = redirects.clone();
        let upstream = Router::new()
            .route(
                "/introspect",
                post(move |Form(form): Form<HashMap<String, String>>| {
                    recorded.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(form["client_id"], "qmcp-resource-server");
                    assert_eq!(form["client_secret"], "introspection-secret");
                    assert_eq!(form["token_type_hint"], "access_token");
                    let reply = current_reply.lock().unwrap().clone();
                    async move {
                        match form["token"].as_str() {
                            "outage" => {
                                (StatusCode::INTERNAL_SERVER_ERROR, "provider-private-body")
                                    .into_response()
                            }
                            "redirect" => (StatusCode::FOUND, [("location", "/must-not-follow")])
                                .into_response(),
                            "oversized" => "x".repeat(MAX_RESPONSE_BYTES + 1).into_response(),
                            "malformed" => "not JSON".into_response(),
                            _ => Json(reply).into_response(),
                        }
                    }
                }),
            )
            .route(
                "/must-not-follow",
                post(move || {
                    leaked.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::BAD_GATEWAY }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/introspect", listener.local_addr().unwrap());
        let provider_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        struct Abort(tokio::task::JoinHandle<()>);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _provider = Abort(provider_task);

        // Private test-only replacement: production constructor enforces HTTPS.
        let mut auth = OAuthAuth::new(config(), "introspection-secret".into()).unwrap();
        let p = Arc::get_mut(&mut auth.0).unwrap();
        p.endpoint = endpoint.parse().unwrap();
        p.client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let server = QmcpServer::new(
            BridgeConfig::new("127.0.0.1:1", false, PermissionPolicy::Cancel).unwrap(),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bind = listener.local_addr().unwrap();
        let router = transport::http_router_with_oauth(server, bind, auth, &[]).unwrap();
        let app = Abort(tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        }));
        let client = Client::builder().no_proxy().build().unwrap();
        let base = format!("http://{bind}");
        for path in [
            "/.well-known/oauth-protected-resource",
            "/.well-known/oauth-protected-resource/mcp",
        ] {
            let metadata = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(metadata.status(), 200);
            let metadata: Value = metadata.json().await.unwrap();
            assert_eq!(metadata["resource"], config().resource);
            assert_eq!(metadata["authorization_servers"], json!([config().issuer]));
            assert_eq!(metadata["scopes_supported"], json!(["qmcp:access"]));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for (header, value) in [
            ("host", "attacker.example.com"),
            ("origin", "https://chatgpt.com"),
        ] {
            assert_eq!(
                client
                    .get(format!("{base}/.well-known/oauth-protected-resource"))
                    .header(header, value)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                403
            );
        }
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}});
        let call = |token: Option<&str>| {
            let mut builder = client
                .post(format!("{base}/mcp"))
                .header("accept", "application/json, text/event-stream")
                .json(&request);
            if let Some(token) = token {
                builder = builder.bearer_auth(token);
            }
            builder.send()
        };
        let response = call(None).await.unwrap();
        assert_eq!(response.status(), 401);
        let challenge = response.headers()[WWW_AUTHENTICATE].to_str().unwrap();
        assert!(challenge.contains(
            "resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource/mcp\""
        ));
        assert!(challenge.contains("scope=\"qmcp:access\""));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "missing credentials must not reach the provider"
        );
        let response = call(Some("active-token")).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap()["result"]["tools"]
                .as_array()
                .unwrap()
                .len(),
            25
        );
        current.lock().unwrap()["active"] = json!(false);
        let response = call(Some("active-token")).await.unwrap();
        assert_eq!(
            response.status(),
            401,
            "revocation must be checked on the next request"
        );
        assert!(
            response.headers()[WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("invalid_token")
        );
        *current.lock().unwrap() = claims();
        current.lock().unwrap()["scope"] = json!("other");
        let response = call(Some("active-token")).await.unwrap();
        assert_eq!(response.status(), 403);
        assert!(
            response.headers()[WWW_AUTHENTICATE]
                .to_str()
                .unwrap()
                .contains("insufficient_scope")
        );
        for token in ["outage", "redirect", "oversized", "malformed"] {
            let response = call(Some(token)).await.unwrap();
            assert_eq!(response.status(), 503);
            assert_eq!(
                response.text().await.unwrap(),
                "authorization service unavailable"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 7);
        assert_eq!(
            redirects.load(Ordering::SeqCst),
            0,
            "provider redirects must not receive tokens or client credentials"
        );
        drop(app);
    }
}
