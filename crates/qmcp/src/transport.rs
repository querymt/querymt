use std::{net::SocketAddr, sync::Arc};

use anyhow::{Result, bail};
use axum::{
    Router,
    extract::{Request, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, CACHE_CONTROL, WWW_AUTHENTICATE},
        uri::Authority,
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::{
    ServiceExt,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
        },
    },
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{
    QmcpServer,
    oauth::{self, OAuthAuth},
};

/// Shared HTTP credential. Only a digest is retained; Debug never reveals it.
#[derive(Clone)]
pub struct BearerToken([u8; 32]);

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BearerToken([redacted])")
    }
}

impl BearerToken {
    /// Validate a nonempty RFC 6750 token without including its value in errors.
    pub fn new(token: &str) -> Result<Self> {
        if !valid_token(token) {
            bail!("bearer token must be 1..4096 ASCII token characters without whitespace");
        }
        Ok(Self(Sha256::digest(token.as_bytes()).into()))
    }

    fn accepts(&self, headers: &HeaderMap) -> bool {
        let Some(credential) = bearer_credential(headers) else {
            return false;
        };
        let actual: [u8; 32] = Sha256::digest(credential.as_bytes()).into();
        bool::from(self.0.ct_eq(&actual))
    }
}

pub(crate) fn bearer_credential(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, credential) = value.split_once(' ')?;
    let credential = credential.trim_start_matches(' ');
    (scheme.eq_ignore_ascii_case("Bearer") && valid_token(credential)).then_some(credential)
}

fn valid_token(value: &str) -> bool {
    let token = value.trim_end_matches('=');
    !token.is_empty()
        && value.len() <= 4096
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
}

async fn require_bearer(
    State(token): State<BearerToken>,
    mut request: Request,
    next: Next,
) -> Response {
    if !token.accepts(request.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            [
                (WWW_AUTHENTICATE, "Bearer realm=\"qmcp\""),
                (CACHE_CONTROL, "no-store"),
            ],
            "unauthorized",
        )
            .into_response();
    }
    // Do not pass credentials into protocol metadata or downstream diagnostics.
    request.headers_mut().remove(AUTHORIZATION);
    next.run(request).await
}

/// Serve MCP over stdio. Only MCP JSON-RPC messages are written to stdout.
pub async fn serve_stdio(server: QmcpServer) -> anyhow::Result<()> {
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}

/// Build the single Streamable HTTP endpoint with JSON responses.
///
/// HTTP is stateless: QueryMT, not the MCP transport, owns agent sessions.
/// Host validation accepts the canonical localhost/127.0.0.1/::1 names; browser
/// Origins are rejected. Use http_router_for_addr for an explicit loopback bind.
pub fn http_router(server: QmcpServer) -> Router {
    build_http_router(server, StreamableHttpServerConfig::default())
}

/// Build HTTP routing for a validated loopback listener, retaining Host/Origin
/// checks while accepting the exact IP chosen by the operator (including 127.0.0.2).
pub fn http_router_for_addr(server: QmcpServer, bind: SocketAddr) -> Result<Router> {
    http_router_with_auth(server, bind, None, &[])
}

/// Build HTTP routing with optional shared-token authentication.
/// Non-loopback binds and custom hosts require a token. Wildcard binds also
/// require explicit hosts. Terminate TLS before exposing the listener remotely.
pub fn http_router_with_auth(
    server: QmcpServer,
    bind: SocketAddr,
    token: Option<BearerToken>,
    allowed_hosts: &[String],
) -> Result<Router> {
    let config = http_config(bind, token.is_some(), allowed_hosts)?;
    let router = build_http_router(server, config);
    Ok(match token {
        Some(token) => router.layer(middleware::from_fn_with_state(token, require_bearer)),
        None => router,
    })
}

/// Protect MCP requests with external-provider OAuth; discovery remains public.
/// TLS must terminate at the public resource URL configured in OAuthAuth.
pub fn http_router_with_oauth(
    server: QmcpServer,
    bind: SocketAddr,
    auth: OAuthAuth,
    allowed_hosts: &[String],
) -> Result<Router> {
    let config = http_config(bind, true, allowed_hosts)?;
    let discovery =
        get(oauth::metadata)
            .with_state(auth.clone())
            .layer(middleware::from_fn_with_state(
                config.allowed_hosts.clone(),
                guard_discovery,
            ));
    Ok(build_http_router(server, config)
        .layer(middleware::from_fn_with_state(auth, oauth::require_oauth))
        .route("/.well-known/oauth-protected-resource", discovery.clone())
        .route("/.well-known/oauth-protected-resource/mcp", discovery))
}

async fn guard_discovery(
    State(hosts): State<Vec<String>>,
    request: Request,
    next: Next,
) -> Response {
    let authority = request
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<Authority>().ok());
    let allowed = authority.is_some_and(|authority| {
        hosts.iter().any(|host| {
            host.eq_ignore_ascii_case(authority.as_str())
                || host
                    .trim_matches(['[', ']'])
                    .eq_ignore_ascii_case(authority.host().trim_matches(['[', ']']))
        })
    });
    if !allowed || request.headers().contains_key("origin") {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

fn http_config(
    bind: SocketAddr,
    authenticated: bool,
    hosts: &[String],
) -> Result<StreamableHttpServerConfig> {
    if !authenticated && (!bind.ip().is_loopback() || !hosts.is_empty()) {
        bail!("non-loopback HTTP binds and custom hosts require a bearer token");
    }
    if bind.ip().is_unspecified() && hosts.is_empty() {
        bail!("wildcard HTTP binds require at least one --allowed-host");
    }
    let mut config = StreamableHttpServerConfig::default();
    if !bind.ip().is_unspecified() {
        config.allowed_hosts.push(bind.ip().to_string());
    }
    for host in hosts {
        let authority = host.parse::<Authority>().ok();
        let valid = !host.is_empty()
            && !host.contains(['*', '@', '/', '?', '#'])
            && !host.chars().any(char::is_whitespace)
            && authority.as_ref().is_some_and(|authority| {
                !authority.host().is_empty()
                    && authority
                        .as_str()
                        .strip_prefix(authority.host())
                        .is_some_and(|suffix| {
                            suffix.is_empty()
                                || suffix
                                    .strip_prefix(':')
                                    .is_some_and(|port| port.parse::<u16>().is_ok())
                        })
                    && !matches!(authority.host(), "0.0.0.0" | "::" | "[::]")
            });
        if !valid {
            bail!(
                "allowed HTTP hosts must be explicit hostnames or IP addresses, optionally with a port"
            );
        }
        config.allowed_hosts.push(host.clone());
    }
    Ok(config)
}

fn build_http_router(server: QmcpServer, config: StreamableHttpServerConfig) -> Router {
    let config = config
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .enforce_origin_validation();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new().nest_service("/mcp", service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BridgeConfig, policy::PermissionPolicy};

    #[test]
    fn bearer_tokens_validate_without_revealing_secrets() {
        for value in [
            "",
            " ",
            "=",
            "contains space",
            "line\nbreak",
            "\u{e9}",
            "bad=padding",
            "bad:token",
        ] {
            let error = BearerToken::new(value).unwrap_err().to_string();
            assert_eq!(
                error,
                "bearer token must be 1..4096 ASCII token characters without whitespace"
            );
        }
        assert!(BearerToken::new(&"x".repeat(4097)).is_err());
        let secret = "test-secret.abc_123-~+/==";
        let token = BearerToken::new(secret).unwrap();
        assert_eq!(format!("{token:?}"), "BearerToken([redacted])");
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, format!("bEaReR  {secret}").parse().unwrap());
        assert!(token.accepts(&headers));
        headers.append(AUTHORIZATION, format!("Bearer {secret}").parse().unwrap());
        assert!(!token.accepts(&headers));
    }

    #[test]
    fn remote_http_requires_auth_and_explicit_wildcard_hosts() {
        assert!(
            http_config(
                "0.0.0.0:3031".parse().unwrap(),
                false,
                &["mcp.example.com".into()]
            )
            .is_err()
        );
        assert!(
            http_config(
                "127.0.0.1:3031".parse().unwrap(),
                false,
                &["mcp.example.com".into()]
            )
            .is_err()
        );
        for address in ["0.0.0.0:3031", "[::]:3031"] {
            let bind = address.parse().unwrap();
            assert!(http_config(bind, true, &[]).is_err());
            let config = http_config(bind, true, &["mcp.example.com:443".into()]).unwrap();
            assert!(config.allowed_hosts.contains(&"mcp.example.com:443".into()));
            assert!(
                !config
                    .allowed_hosts
                    .iter()
                    .any(|h| h == "0.0.0.0" || h == "::")
            );
        }
        let config = http_config("192.0.2.10:3031".parse().unwrap(), true, &[]).unwrap();
        assert!(config.allowed_hosts.contains(&"192.0.2.10".into()));
        for host in [
            "",
            "*",
            "*.example.com",
            "http://mcp.example.com",
            "user@mcp.example.com",
            "bad host",
            "example.com:65536",
            "0.0.0.0",
            "[::]",
        ] {
            assert!(
                http_config("0.0.0.0:3031".parse().unwrap(), true, &[host.into()]).is_err(),
                "must reject {host}"
            );
        }
    }

    #[test]
    fn explicit_http_router_accepts_only_loopback_bind_addresses() {
        let server = QmcpServer::new(
            BridgeConfig::new("127.0.0.1:1", false, PermissionPolicy::Cancel).unwrap(),
        )
        .unwrap();
        for address in ["127.0.0.1:3031", "127.0.0.2:3031", "[::1]:3031"] {
            assert!(
                http_router_for_addr(server.clone(), address.parse().unwrap()).is_ok(),
                "{address}"
            );
        }
        for address in [
            "0.0.0.0:3031",
            "192.0.2.1:3031",
            "[::]:3031",
            "[2001:db8::1]:3031",
        ] {
            assert!(
                http_router_for_addr(server.clone(), address.parse().unwrap()).is_err(),
                "{address}"
            );
        }
    }
}
