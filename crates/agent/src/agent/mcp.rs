//! MCP (Model Context Protocol) integration utilities

use crate::acp::protocol::{Error, HttpHeader, McpServer};
use crate::config::{McpAuth, McpServerConfig};
use anyhow::{Result as AnyResult, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// Converts HTTP headers from protocol format to reqwest format
pub fn headers_to_map(headers: &[HttpHeader]) -> Result<HeaderMap, Error> {
    let mut map = HeaderMap::new();
    for header in headers {
        let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({
                "message": "invalid header name",
                "name": header.name,
                "error": e.to_string(),
            }))
        })?;
        let value = HeaderValue::from_str(&header.value).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({
                "message": "invalid header value",
                "name": header.name,
                "error": e.to_string(),
            }))
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

/// Gets the name of an MCP server configuration
pub fn get_mcp_server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Stdio(stdio) => &stdio.name,
        McpServer::Http(http) => &http.name,
        _ => "unknown",
    }
}

/// Gets the URL from an MCP server configuration if available
pub fn get_mcp_server_url(server: &McpServer) -> Option<&str> {
    match server {
        McpServer::Http(http) => Some(&http.url),
        _ => None,
    }
}

/// Gets the headers from an MCP server configuration if available
pub fn get_mcp_server_headers(server: &McpServer) -> &[HttpHeader] {
    match server {
        McpServer::Http(http) => &http.headers,
        _ => &[],
    }
}

/// Checks if an MCP server configuration requires network access
pub fn mcp_server_requires_network(server: &McpServer) -> bool {
    matches!(server, McpServer::Http(_))
}

/// Validate MCP server settings that must hold wherever the server came from.
///
/// Called for file configs (via the config loader) and for ACP-supplied
/// servers at connect time, since the latter bypass the loader. Rejects, with
/// an error naming the server:
/// - a `socket_path` that is empty, exactly `"@"`, or not valid UTF-8
///   (`UnixSocketHttpClient::new` panics on the first two);
/// - `socket_path` combined with `auth`;
/// - `auth` combined with an `Authorization` header (case-insensitive).
///
/// # Errors
/// Returns the first violated rule.
pub fn validate_mcp_server(server: &McpServerConfig) -> AnyResult<()> {
    let McpServerConfig::Http {
        name,
        headers,
        auth,
        socket_path,
        ..
    } = server
    else {
        return Ok(());
    };
    if let Some(path) = socket_path {
        match path.to_str() {
            None => bail!("MCP server `{name}`: `socket_path` must be valid UTF-8"),
            Some("" | "@") => bail!("MCP server `{name}`: `socket_path` must not be empty or `@`"),
            Some(_) => {}
        }
        if auth.is_some() {
            bail!("MCP server `{name}`: `socket_path` cannot be combined with `auth`");
        }
    }
    if auth.is_some()
        && headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
    {
        bail!("MCP server `{name}`: `auth` cannot be combined with an `Authorization` header");
    }
    Ok(())
}

/// An OAuth HTTP server declared in the agent configuration.
#[derive(Debug, Clone)]
struct ConfiguredOAuthServer {
    name: String,
    url: String,
    auth: McpAuth,
}

/// Connect-time context shared by every server of a session.
///
/// Carries the OAuth servers declared in the **agent configuration**. OAuth is
/// only ever applied to a session server that matches one of them, so a
/// client-supplied server cannot borrow stored credentials for another URL or
/// bring its own OAuth settings.
#[derive(Clone)]
pub(crate) struct McpConnectContext {
    oauth_servers: Vec<ConfiguredOAuthServer>,
    /// Where OAuth credentials are read from (the system keyring by default).
    secret_backend: std::sync::Arc<dyn crate::mcp_auth::SecretBackend>,
}

impl Default for McpConnectContext {
    fn default() -> Self {
        Self {
            oauth_servers: Vec::new(),
            secret_backend: std::sync::Arc::new(crate::mcp_auth::KeyringBackend),
        }
    }
}

impl std::fmt::Debug for McpConnectContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnectContext")
            .field("oauth_servers", &self.oauth_servers)
            .finish_non_exhaustive()
    }
}

impl McpConnectContext {
    /// Use a different credential backend (tests).
    #[cfg(test)]
    pub(crate) fn with_secret_backend(
        mut self,
        backend: std::sync::Arc<dyn crate::mcp_auth::SecretBackend>,
    ) -> Self {
        self.secret_backend = backend;
        self
    }

    /// The credential backend for OAuth MCP servers.
    #[cfg_attr(not(feature = "oauth"), allow(dead_code))]
    pub(crate) fn secret_backend(&self) -> std::sync::Arc<dyn crate::mcp_auth::SecretBackend> {
        self.secret_backend.clone()
    }

    /// Collect the OAuth HTTP servers from the agent configuration.
    pub(crate) fn from_config(servers: &[McpServerConfig]) -> Self {
        let oauth_servers = servers
            .iter()
            .filter_map(|server| match server {
                McpServerConfig::Http {
                    name,
                    url,
                    auth: Some(auth),
                    ..
                } => Some(ConfiguredOAuthServer {
                    name: name.clone(),
                    url: url.clone(),
                    auth: auth.clone(),
                }),
                _ => None,
            })
            .collect();
        Self {
            oauth_servers,
            ..Self::default()
        }
    }

    /// Decide the OAuth settings for a session server (D8 scope rules).
    ///
    /// - A server that does not request OAuth gets none. This covers a
    ///   client-supplied server overriding an OAuth config server by name with
    ///   a different URL: it connects without OAuth credentials.
    /// - A server that requests OAuth and matches an agent-config OAuth server
    ///   by name and normalised URL gets the **agent config's** settings, never
    ///   those carried by the session server.
    /// - A server that requests OAuth but shares its name with an OAuth config
    ///   server (different URL, e.g. a copied config entry) also connects
    ///   without OAuth.
    /// - A server that requests OAuth without any such name is rejected.
    ///
    /// # Errors
    /// Returns an error naming the server when OAuth is requested without a
    /// matching agent-config declaration.
    pub(crate) fn resolve_oauth(&self, server: &McpServerConfig) -> AnyResult<Option<McpAuth>> {
        let McpServerConfig::Http {
            name,
            url,
            auth: Some(_),
            ..
        } = server
        else {
            return Ok(None);
        };
        if let Some(configured) = self
            .oauth_servers
            .iter()
            .find(|c| c.name == *name && crate::mcp_auth::same_url(&c.url, url))
        {
            return Ok(Some(configured.auth.clone()));
        }
        // An override of an agent-config OAuth server by name with another URL
        // connects without OAuth even if it still carries the `auth` setting
        // (e.g. copied from the config entry).
        if self.oauth_servers.iter().any(|c| c.name == *name) {
            return Ok(None);
        }
        bail!("OAuth MCP servers must be declared in the agent configuration: {name}")
    }
}

/// Whether a failed MCP tool call needs a new login: the transport reported
/// that authorization is required (no usable credentials, a rejected refresh
/// token, or a token the server no longer accepts).
pub(crate) fn is_auth_required_error(err: &anyhow::Error) -> bool {
    use querymt::mcp::adapter::McpToolError;
    use rmcp::service::ServiceError;
    let Some(McpToolError::Service(ServiceError::TransportSend(transport))) =
        err.downcast_ref::<McpToolError>()
    else {
        return false;
    };
    let mut current = Some(&*transport.error as &(dyn std::error::Error + 'static));
    while let Some(e) = current {
        #[cfg(feature = "oauth")]
        if matches!(
            e.downcast_ref::<rmcp::transport::AuthError>(),
            Some(
                rmcp::transport::AuthError::AuthorizationRequired
                    | rmcp::transport::AuthError::TokenRefreshRejected(_)
            )
        ) {
            return true;
        }
        if e.is::<rmcp::transport::streamable_http_client::AuthRequiredError>() {
            return true;
        }
        current = e.source();
    }
    false
}

/// Tool-result text for a call that failed because `server` needs a login.
pub(crate) fn auth_required_text(server: &str) -> String {
    format!(
        "Error: authorization required for MCP server {server}; log in with provider {}",
        crate::mcp_auth::provider_id(server)
    )
}

/// Map an MCP tool-call failure on an OAuth server that needs a login to the
/// user-facing error text, and tell the session's clients via the transient
/// [`McpServerAuthRequired`](crate::events::AgentEventKind::McpServerAuthRequired)
/// event. Returns `None` for every other failure.
pub(crate) fn report_auth_required(
    err: &anyhow::Error,
    server: &str,
    is_oauth_server: bool,
    event_sink: &crate::event_sink::EventSink,
    session_id: &str,
) -> Option<String> {
    if !is_oauth_server || !is_auth_required_error(err) {
        return None;
    }
    event_sink.emit_ephemeral(
        session_id,
        crate::events::AgentEventKind::McpServerAuthRequired {
            server_name: server.to_string(),
            provider_id: crate::mcp_auth::provider_id(server),
            error: None,
        },
    );
    Some(auth_required_text(server))
}

/// Returns the standard `Implementation` descriptor for the querymt-agent.
pub fn agent_implementation() -> rmcp::model::Implementation {
    rmcp::model::Implementation::new("querymt-agent", env!("QMT_BUILD_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpAuth;

    fn oauth() -> McpAuth {
        McpAuth::OAuth {
            client_id: None,
            scopes: vec![],
            redirect_port: None,
        }
    }

    fn err(server: McpServerConfig) -> String {
        validate_mcp_server(&server).unwrap_err().to_string()
    }

    #[test]
    fn accepts_valid_servers() {
        validate_mcp_server(&McpServerConfig::stdio("s", "cmd")).unwrap();
        validate_mcp_server(&McpServerConfig::http("h", "https://e.test")).unwrap();
        validate_mcp_server(
            &McpServerConfig::http("h", "http://l/").with_socket_path("/run/a.sock"),
        )
        .unwrap();
        validate_mcp_server(&McpServerConfig::http("h", "https://e.test").with_auth(oauth()))
            .unwrap();
        // Abstract-namespace sockets ("@name") are legitimate.
        validate_mcp_server(&McpServerConfig::http("h", "http://l/").with_socket_path("@abs"))
            .unwrap();
    }

    #[test]
    fn rejects_empty_and_bare_at_socket_path() {
        for path in ["", "@"] {
            let msg = err(McpServerConfig::http("sock", "http://l/").with_socket_path(path));
            assert!(
                msg.contains("`sock`") && msg.contains("socket_path"),
                "{msg}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_socket_path() {
        use std::os::unix::ffi::OsStrExt;
        let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&[0x2f, 0xff, 0xfe]));
        let msg = err(McpServerConfig::http("sock", "http://l/").with_socket_path(path));
        assert!(msg.contains("`sock`") && msg.contains("UTF-8"), "{msg}");
    }

    #[test]
    fn rejects_socket_path_with_auth() {
        let msg = err(McpServerConfig::http("both", "http://l/")
            .with_socket_path("/run/a.sock")
            .with_auth(oauth()));
        assert!(
            msg.contains("`both`") && msg.contains("socket_path"),
            "{msg}"
        );
    }

    #[test]
    fn rejects_auth_with_authorization_header_case_insensitively() {
        for key in ["Authorization", "AUTHORIZATION", "authorization"] {
            let msg = err(McpServerConfig::http("hdr", "https://e.test")
                .with_auth(oauth())
                .with_headers([(key.to_string(), "Bearer x".to_string())].into()));
            assert!(
                msg.contains("`hdr`") && msg.contains("Authorization"),
                "{msg}"
            );
        }
    }

    fn oauth_with(scopes: &[&str]) -> McpAuth {
        McpAuth::OAuth {
            client_id: Some("cfg".into()),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            redirect_port: Some(5000),
        }
    }

    fn ctx() -> McpConnectContext {
        McpConnectContext::from_config(&[
            McpServerConfig::http("linear", "https://mcp.linear.app/mcp")
                .with_auth(oauth_with(&["read"])),
            McpServerConfig::http("plain", "https://plain.test/mcp"),
            McpServerConfig::stdio("local", "cmd"),
        ])
    }

    #[test]
    fn oauth_server_matching_name_and_url_uses_agent_config_settings() {
        // The session server (e.g. client-supplied) carries different settings;
        // the agent config's win. URL comparison is normalised.
        let session = McpServerConfig::http("linear", "https://MCP.linear.app/mcp")
            .with_auth(oauth_with(&["admin"]));
        assert_eq!(
            ctx().resolve_oauth(&session).unwrap(),
            Some(oauth_with(&["read"]))
        );

        // The config server itself round-trips through ACP and still matches.
        let configured = McpServerConfig::from_acp(
            &McpServerConfig::http("linear", "https://mcp.linear.app/mcp")
                .with_auth(oauth_with(&["read"]))
                .to_acp(),
        )
        .unwrap();
        assert_eq!(
            ctx().resolve_oauth(&configured).unwrap(),
            Some(oauth_with(&["read"]))
        );
    }

    #[test]
    fn client_supplied_oauth_without_a_match_fails_naming_the_server() {
        for session in [
            // Not declared in the agent config at all.
            McpServerConfig::http("other", "https://other.test/mcp").with_auth(oauth()),
            // Declared, but without OAuth.
            McpServerConfig::http("plain", "https://plain.test/mcp").with_auth(oauth()),
        ] {
            let name = session.name().to_string();
            let msg = ctx().resolve_oauth(&session).unwrap_err().to_string();
            assert_eq!(
                msg,
                format!("OAuth MCP servers must be declared in the agent configuration: {name}")
            );
        }
        // An empty context (no OAuth servers configured) rejects too.
        let session =
            McpServerConfig::http("linear", "https://mcp.linear.app/mcp").with_auth(oauth());
        assert!(
            McpConnectContext::default()
                .resolve_oauth(&session)
                .is_err()
        );
    }

    #[test]
    fn different_url_override_carrying_auth_connects_without_oauth() {
        let session = McpServerConfig::http("linear", "https://evil.test/mcp").with_auth(oauth());
        assert_eq!(ctx().resolve_oauth(&session).unwrap(), None);
    }

    #[test]
    fn different_url_override_connects_without_oauth() {
        let session = McpServerConfig::http("linear", "https://mirror.test/mcp");
        assert_eq!(ctx().resolve_oauth(&session).unwrap(), None);
    }

    #[test]
    fn servers_without_oauth_get_none() {
        let context = ctx();
        for session in [
            McpServerConfig::http("plain", "https://plain.test/mcp"),
            McpServerConfig::stdio("local", "cmd"),
            // Same name and URL as the OAuth config server, but not asking
            // for OAuth: static headers keep working as before.
            McpServerConfig::http("linear", "https://mcp.linear.app/mcp"),
        ] {
            assert_eq!(context.resolve_oauth(&session).unwrap(), None);
        }
    }

    #[test]
    fn headers_without_auth_are_fine() {
        validate_mcp_server(
            &McpServerConfig::http("h", "https://e.test")
                .with_headers([("Authorization".to_string(), "Bearer x".to_string())].into()),
        )
        .unwrap();
    }
}
