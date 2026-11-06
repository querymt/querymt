//! Agent Client Protocol helper functions for MCP server management
use crate::acp::protocol::{Error, McpServer, McpServerHttp, McpServerStdio};
use crate::agent::core::McpToolState;
use crate::agent::core::SkippedMcpServer;
pub(crate) use crate::agent::mcp::McpConnectContext;
use crate::agent::session_mcp::ConnectedMcpPeer;
use crate::error::AgentError;
use log::warn;
use querymt::tool_decorator::CallFunctionTool;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::{
    RoleClient,
    model::Implementation,
    service::{RunningService, serve_client},
    transport::{
        IntoTransport, StreamableHttpClientTransport, child_process::TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::collections::HashMap;
use std::sync::Arc;

/// Builds MCP state by connecting to configured servers.
///
/// Populates `tool_state` with the initial tool list fetched from each server.
/// Each [`McpClientHandler`][crate::elicitation::McpClientHandler] created here
/// holds a clone of `tool_state` so it can refresh the maps live when the server
/// sends a `tools/list_changed` notification.
///
/// `connect_ctx` carries the agent-config OAuth servers; OAuth is applied to a
/// session server only when it matches one of them.
///
/// Returns the `RunningService` map (must be kept alive to maintain connections).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_mcp_state(
    servers: &[McpServer],
    connect_ctx: &McpConnectContext,
    pending_elicitations: crate::elicitation::PendingElicitationMap,
    event_sink: Arc<crate::event_sink::EventSink>,
    session_id: String,
    client_impl: &Implementation,
    tool_state: Arc<McpToolState>,
    cwd: Option<&std::path::Path>,
) -> Result<HashMap<String, RunningService<RoleClient, crate::elicitation::McpClientHandler>>, Error>
{
    let mut clients = HashMap::new();
    let mut tools = HashMap::new();
    let mut tool_defs = Vec::new();
    let mut skipped_servers = Vec::new();

    for server in servers {
        let (server_name, running): (
            String,
            RunningService<RoleClient, crate::elicitation::McpClientHandler>,
        ) = match start_mcp_server(
            server,
            connect_ctx,
            pending_elicitations.clone(),
            event_sink.clone(),
            session_id.clone(),
            client_impl,
            tool_state.clone(),
            cwd,
        )
        .await
        {
            Ok(started) => started,
            // An OAuth server without usable credentials, or whose
            // authorization cannot be checked, is skipped so the session still
            // starts; the problem is logged and reported to the client. Every
            // other failure keeps failing session setup.
            Err(err) => {
                let skipped = if let Some(server_name) =
                    crate::error::mcp_auth_required_server(&err)
                {
                    warn!(
                        "MCP server '{server_name}' requires authorization; skipping it \
                         (log in with provider {})",
                        crate::mcp_auth::provider_id(server_name)
                    );
                    SkippedMcpServer::auth_required(server_name)
                } else if let Some((server_name, reason)) = crate::error::mcp_auth_unavailable(&err)
                {
                    log::error!(
                        "MCP server '{server_name}' skipped: authorization is unavailable: {reason}"
                    );
                    SkippedMcpServer::auth_unavailable(server_name, reason)
                } else {
                    return Err(err);
                };
                event_sink.emit_ephemeral(
                    &session_id,
                    crate::events::AgentEventKind::McpServerAuthRequired {
                        server_name: skipped.server_name.clone(),
                        provider_id: skipped.provider_id.clone(),
                        error: skipped.error.clone(),
                    },
                );
                skipped_servers.push(skipped);
                continue;
            }
        };
        let peer = running.peer().clone();
        let router = running.service().router();
        let tool_list = peer
            .list_all_tools()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        for tool in tool_list {
            let adapter = querymt::mcp::adapter::McpToolAdapter::try_new(
                tool,
                peer.clone(),
                server_name.clone(),
            )
            .map_err(|e| Error::internal_error().data(e.to_string()))?
            .with_router(router.clone());
            let name = adapter.descriptor().function.name.clone();
            if tools.contains_key(&name) {
                warn!("Duplicate MCP tool '{}', keeping first instance", name);
                continue;
            }
            tool_defs.push(adapter.descriptor());
            tools.insert(name, Arc::new(adapter));
        }
        clients.insert(server_name, running);
    }

    // Skipped servers are also recorded on the state: events emitted while the
    // session is being created can precede the client's subscription, so the
    // session/new and session/load responses disclose them too.
    tool_state.set_skipped_servers(skipped_servers);

    // Populate the shared tool state with the initial tool list.
    let snapshot = crate::agent::core::McpToolSnapshot {
        tools,
        tool_defs,
        tools_hash: None,
    };
    crate::agent::mcp::warn_unmatched_permissions(connect_ctx.permissions(), &snapshot, None);
    tool_state.store(snapshot);

    Ok(clients)
}

/// Merge tools from already-connected MCP peers into an existing tool state.
///
/// The peers are already initialized and can be reused across sessions without
/// re-initializing.
pub(crate) async fn merge_connected_mcp_peers(
    tool_state: Arc<McpToolState>,
    connected_peers: &[ConnectedMcpPeer],
) -> Result<(), Error> {
    if connected_peers.is_empty() {
        return Ok(());
    }

    let current = tool_state.load();
    let mut tools = current.tools.clone();
    let mut tool_defs = current.tool_defs.clone();

    for connected in connected_peers {
        let tool_list = connected
            .peer
            .list_all_tools()
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        for tool in tool_list {
            let adapter = querymt::mcp::adapter::McpToolAdapter::try_new(
                tool,
                connected.peer.clone(),
                connected.server_name.clone(),
            )
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
            let name = adapter.descriptor().function.name.clone();
            if tools.contains_key(&name) {
                warn!("Duplicate MCP tool '{}', keeping first instance", name);
                continue;
            }
            tool_defs.push(adapter.descriptor());
            tools.insert(name, Arc::new(adapter));
        }
    }

    tool_state.store(crate::agent::core::McpToolSnapshot {
        tools,
        tool_defs,
        tools_hash: None,
    });

    Ok(())
}

/// Starts an MCP server based on its configuration.
/// Build the stdio child-process command for an MCP server, running inside
/// the session's effective workspace when one is set so workspace-relative
/// server entries resolve against the bound checkout rather than the agent
/// process's working directory.
fn stdio_command(stdio: &McpServerStdio, cwd: Option<&std::path::Path>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(&stdio.command);
    cmd.args(stdio.args.iter())
        .envs(stdio.env.iter().map(|item| (&item.name, &item.value)))
        .stderr(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());
    // Only an existing directory: a stale stored cwd must not turn into a
    // spawn failure for a server that never depended on it.
    if let Some(dir) = cwd.filter(|dir| dir.is_dir()) {
        cmd.current_dir(dir);
    }
    cmd
}

/// Initialise an MCP client connection over `transport`.
///
/// With `cache` off, response caching is disabled right after the handshake
/// and before the first `list_all_tools`, so even the first listing hits the
/// server.
pub(crate) async fn serve_mcp_client<T, E, A>(
    handler: crate::elicitation::McpClientHandler,
    transport: T,
    transport_label: &str,
    cache: bool,
) -> Result<RunningService<RoleClient, crate::elicitation::McpClientHandler>, Error>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let running = serve_client(handler, transport).await.map_err(|e| {
        Error::from(AgentError::McpServerFailed {
            transport: transport_label.to_string(),
            reason: e.to_string(),
        })
    })?;
    if !cache {
        running
            .peer()
            .set_response_cache_config(rmcp::service::ClientCacheConfig::disabled())
            .await;
    }
    Ok(running)
}

#[allow(clippy::too_many_arguments)]
async fn start_mcp_server(
    server: &McpServer,
    connect_ctx: &McpConnectContext,
    pending_elicitations: crate::elicitation::PendingElicitationMap,
    event_sink: Arc<crate::event_sink::EventSink>,
    session_id: String,
    client_impl: &Implementation,
    tool_state: Arc<McpToolState>,
    cwd: Option<&std::path::Path>,
) -> Result<
    (
        String,
        RunningService<RoleClient, crate::elicitation::McpClientHandler>,
    ),
    Error,
> {
    // ACP-supplied servers bypass the config loader, so validate here.
    let config = crate::config::McpServerConfig::from_acp(server)
        .map_err(|e| Error::invalid_params().data(e.to_string()))?;
    crate::agent::mcp::validate_mcp_server(&config)
        .map_err(|e| Error::invalid_params().data(e.to_string()))?;
    // D8 scope rules: OAuth applies only to servers matching an agent-config
    // OAuth server (name + normalised URL), with the config's settings.
    let oauth = connect_ctx
        .resolve_oauth(&config)
        .map_err(|e| Error::invalid_params().data(e.to_string()))?;
    let handler = |name: &str| {
        crate::elicitation::McpClientHandler::new(
            pending_elicitations.clone(),
            event_sink.clone(),
            name.to_string(),
            session_id.clone(),
            client_impl.clone(),
            tool_state.clone(),
        )
        .with_tasks(config.tasks_enabled())
        .with_permissions(connect_ctx.permissions().clone())
    };
    match server {
        McpServer::Stdio(stdio) => {
            let McpServerStdio { name, .. } = stdio;
            let cmd = stdio_command(stdio, cwd);
            let transport = TokioChildProcess::new(cmd)
                .map_err(|e| Error::internal_error().data(e.to_string()))?;
            let running =
                serve_mcp_client(handler(name), transport, "stdio", config.cache_enabled()).await?;
            Ok((name.clone(), running))
        }
        McpServer::Http(http) => {
            let McpServerHttp {
                name, url, headers, ..
            } = http.clone();
            let header_map = headers_to_map(&headers)?;
            let transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            let cache = config.cache_enabled();
            let running = match (oauth, config.socket_path()) {
                (Some(auth), _) => {
                    let client = reqwest::ClientBuilder::new()
                        .default_headers(header_map)
                        .build()
                        .map_err(|e| Error::internal_error().data(e.to_string()))?;
                    let transport =
                        oauth_transport(connect_ctx, &name, &url, &auth, client, transport_config)
                            .await?;
                    let running =
                        serve_mcp_client(handler(&name), transport, "http", cache).await?;
                    // OAuth was really applied: only now can a 401 mean "log in".
                    tool_state.mark_oauth_server(&name);
                    running
                }
                (None, Some(socket_path)) => {
                    let transport = unix_socket_transport(
                        &name,
                        socket_path,
                        &url,
                        header_map,
                        transport_config,
                    )?;
                    serve_mcp_client(handler(&name), transport, "http", cache).await?
                }
                (None, None) => {
                    let client = reqwest::ClientBuilder::new()
                        .default_headers(header_map)
                        .build()
                        .map_err(|e| Error::internal_error().data(e.to_string()))?;
                    let transport =
                        StreamableHttpClientTransport::with_client(client, transport_config);
                    serve_mcp_client(handler(&name), transport, "http", cache).await?
                }
            };
            Ok((name, running))
        }
        _ => Err(Error::invalid_params().data(serde_json::json!({
            "message": "unsupported MCP server configuration",
        }))),
    }
}

/// Streamable HTTP authorized with stored OAuth credentials.
///
/// The credentials are loaded (and refreshed on demand) through the secret
/// store, bound to `url`. Without usable credentials the server cannot be
/// connected and the user must log in with provider `mcp:<name>`.
///
/// Static headers go on `client` only; they are never sent to the
/// authorization server, which uses rmcp's own HTTP client.
#[cfg(feature = "oauth")]
async fn oauth_transport(
    connect_ctx: &McpConnectContext,
    name: &str,
    url: &str,
    auth: &crate::config::McpAuth,
    client: reqwest::Client,
    config: StreamableHttpClientTransportConfig,
) -> Result<StreamableHttpClientTransport<rmcp::transport::AuthClient<reqwest::Client>>, Error> {
    use rmcp::transport::AuthorizationManager;
    // Anything that stops us from *checking* authorization (discovery, the
    // authorization server being down, an unreadable store) skips this server
    // and is reported, instead of failing the whole session.
    let unavailable = |reason: String| {
        Error::from(AgentError::McpAuthUnavailable {
            server: name.to_string(),
            reason,
        })
    };
    let crate::config::McpAuth::OAuth { redirect_port, .. } = auth;
    let store = crate::mcp_auth::SecretStoreCredentialStore::with_backend(
        name,
        url,
        crate::mcp_auth::redirect_uri(*redirect_port),
        connect_ctx.secret_backend(),
    )
    .with_auth_policy(auth)
    .for_connection();
    // The OAuth client sends a `User-Agent` (some authorization servers refuse
    // requests without one) and, like `new_allowing_private_addresses`, lets
    // the discovered authorization server be on a private address: internal MCP
    // servers commonly sit behind an internal identity provider.
    let http = crate::mcp_auth::oauth_http_client().map_err(|e| unavailable(e.to_string()))?;
    let mut manager = AuthorizationManager::new_with_oauth_http_client(url, http)
        .await
        .map_err(|e| unavailable(e.to_string()))?;
    manager.set_credential_store(store);
    let have_credentials = manager
        .initialize_from_store()
        .await
        .map_err(|e| unavailable(e.to_string()))?;
    if !have_credentials {
        return Err(Error::from(AgentError::McpAuthRequired {
            server: name.to_string(),
        }));
    }
    Ok(StreamableHttpClientTransport::with_client(
        rmcp::transport::AuthClient::new(client, manager),
        config,
    ))
}

/// OAuth needs the `oauth` cargo feature.
#[cfg(not(feature = "oauth"))]
async fn oauth_transport(
    _connect_ctx: &McpConnectContext,
    _name: &str,
    _url: &str,
    _auth: &crate::config::McpAuth,
    _client: reqwest::Client,
    _config: StreamableHttpClientTransportConfig,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, Error> {
    Err(Error::from(AgentError::McpServerFailed {
        transport: "http".to_string(),
        reason: "OAuth support is not enabled in this build".to_string(),
    }))
}

/// Streamable HTTP over a Unix domain socket. `url` is used for request
/// addressing and the `Host` header; headers travel as custom headers.
///
/// `socket_path` was validated by [`crate::agent::mcp::validate_mcp_server`],
/// so rmcp's constructor cannot panic on it.
#[cfg(unix)]
fn unix_socket_transport(
    _name: &str,
    socket_path: &std::path::Path,
    url: &str,
    headers: HeaderMap,
    config: StreamableHttpClientTransportConfig,
) -> Result<StreamableHttpClientTransport<rmcp::transport::UnixSocketHttpClient>, Error> {
    let socket_path = socket_path.to_str().ok_or_else(|| {
        Error::invalid_params().data(serde_json::json!({
            "message": "socket_path must be valid UTF-8",
        }))
    })?;
    let client = rmcp::transport::UnixSocketHttpClient::new(socket_path, url);
    // `custom_headers` holds one value per name and rmcp rejects the names it
    // manages itself, so a header set more than once keeps its last value and
    // a reserved name fails here with the server named (it would be accepted
    // over TCP, where headers are reqwest defaults).
    let mut custom_headers = std::collections::HashMap::new();
    for (name, value) in headers {
        let Some(name) = name else { continue };
        if matches!(
            name.as_str(),
            "accept" | "mcp-session-id" | "mcp-protocol-version" | "last-event-id"
        ) {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": format!(
                    "MCP server {_name}: header `{name}` is managed by the transport and cannot be set with socket_path"
                ),
            })));
        }
        custom_headers.insert(name, value);
    }
    Ok(StreamableHttpClientTransport::with_client(
        client,
        config.custom_headers(custom_headers),
    ))
}

/// Unix sockets are unavailable on this platform.
#[cfg(not(unix))]
fn unix_socket_transport(
    name: &str,
    _socket_path: &std::path::Path,
    _url: &str,
    _headers: HeaderMap,
    _config: StreamableHttpClientTransportConfig,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, Error> {
    Err(Error::from(AgentError::McpServerFailed {
        transport: "http".to_string(),
        reason: unix_socket_unsupported(name),
    }))
}

/// Error text for a socket-configured server on a platform without sockets.
#[cfg(any(not(unix), test))]
fn unix_socket_unsupported(name: &str) -> String {
    format!("Unix sockets are unsupported for MCP server {name} on this platform")
}

/// Converts HTTP headers from protocol format to reqwest format
fn headers_to_map(headers: &[crate::acp::protocol::HttpHeader]) -> Result<HeaderMap, Error> {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn stdio_command_runs_inside_the_session_workspace() {
        let server = McpServerStdio::new("test", "/bin/true").args(vec!["--x".to_string()]);
        let dir = tempfile::TempDir::new().unwrap();

        // Workspace-bound sessions spawn MCP servers inside the bound
        // checkout so workspace-relative entries resolve against it.
        let cmd = stdio_command(&server, Some(dir.path()));
        assert_eq!(cmd.as_std().get_current_dir(), Some(dir.path()));

        // Local sessions keep the agent process working directory.
        let cmd = stdio_command(&server, None);
        assert_eq!(cmd.as_std().get_current_dir(), None);

        // A stale (missing) workspace directory is ignored rather than
        // failing the spawn.
        let missing = dir.path().join("does-not-exist");
        let cmd = stdio_command(&server, Some(&missing));
        assert_eq!(cmd.as_std().get_current_dir(), None);
    }

    #[tokio::test]
    async fn client_supplied_empty_socket_path_fails_that_server_without_panicking() {
        use crate::config::McpServerConfig;
        use crate::session::backend::StorageBackend;
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let sink = Arc::new(crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(crate::event_fanout::EventFanout::new()),
        ));
        for path in ["", "@"] {
            let server = McpServerConfig::http("bad-sock", "http://localhost/mcp")
                .with_socket_path(path)
                .to_acp();
            let Err(err) = build_mcp_state(
                &[server],
                &McpConnectContext::default(),
                Default::default(),
                sink.clone(),
                "s1".into(),
                &crate::agent::mcp::agent_implementation(),
                McpToolState::empty(),
                None,
            )
            .await
            else {
                panic!("invalid socket_path must fail the server");
            };
            assert!(format!("{err:?}").contains("bad-sock"), "{err:?}");
        }
    }

    #[tokio::test]
    async fn client_supplied_oauth_without_config_match_fails_that_server() {
        use crate::config::{McpAuth, McpServerConfig};
        use crate::session::backend::StorageBackend;
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let sink = Arc::new(crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(crate::event_fanout::EventFanout::new()),
        ));
        let server = McpServerConfig::http("other", "https://other.test/mcp")
            .with_auth(McpAuth::OAuth {
                client_id: None,
                scopes: vec![],
                redirect_port: None,
                strict_scopes: false,
            })
            .to_acp();
        let Err(err) = build_mcp_state(
            &[server],
            &McpConnectContext::default(),
            Default::default(),
            sink,
            "s1".into(),
            &crate::agent::mcp::agent_implementation(),
            McpToolState::empty(),
            None,
        )
        .await
        else {
            panic!("unmatched client-supplied OAuth must fail the server");
        };
        assert!(
            format!("{err:?}")
                .contains("OAuth MCP servers must be declared in the agent configuration: other"),
            "{err:?}"
        );
    }

    /// OAuth connect (task 8.3) against a mock Streamable HTTP MCP server that
    /// requires a bearer token and serves a refresh-capable token endpoint.
    #[cfg(feature = "oauth")]
    pub(crate) mod oauth_connect {
        use super::*;
        use crate::config::{McpAuth, McpServerConfig};
        use crate::mcp_auth::SecretStoreCredentialStore;
        use crate::mcp_auth::test_support::{MemoryBackend, creds};
        use crate::session::backend::StorageBackend;
        use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
        use rmcp::transport::{CredentialStore, StoredCredentials};

        #[derive(Default)]
        pub(crate) struct Mock {
            /// Bearer tokens seen on `/mcp` requests.
            pub(crate) bearers: parking_lot::Mutex<Vec<String>>,
            pub(crate) refreshes: std::sync::atomic::AtomicUsize,
            /// `tools/call` answers 401 (token revoked server-side).
            pub(crate) revoked: std::sync::atomic::AtomicBool,
            /// The token endpoint answers `invalid_grant`.
            pub(crate) reject_refresh: std::sync::atomic::AtomicBool,
        }

        async fn mcp(
            State(mock): State<Arc<Mock>>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let bearer = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            mock.bearers.lock().push(bearer.clone());
            if bearer != "Bearer good-token" && bearer != "Bearer fresh-token" {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [("www-authenticate", "Bearer")],
                )
                    .into_response();
            }
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            if body.get("method").and_then(|m| m.as_str()) == Some("tools/call")
                && mock.revoked.load(std::sync::atomic::Ordering::SeqCst)
            {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [("www-authenticate", "Bearer")],
                )
                    .into_response();
            }
            let payload = match body.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
                    "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "oauth-test", "version": "1"}}}),
                Some("tools/list") => serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
                    "result": {"tools": [{"name": "secret", "inputSchema": {"type": "object"}}]}}),
                Some(m) if m.starts_with("notifications/") => {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                }
                _ => serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": {}}),
            };
            Json(payload).into_response()
        }

        async fn token(State(mock): State<Arc<Mock>>) -> axum::response::Response {
            use axum::response::IntoResponse;
            mock.refreshes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if mock
                .reject_refresh
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid_grant"})),
                )
                    .into_response();
            }
            Json(serde_json::json!({
                "access_token": "fresh-token",
                "token_type": "bearer",
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            }))
            .into_response()
        }

        /// Serve the mock; returns its base URL (no path).
        pub(crate) async fn serve(mock: Arc<Mock>) -> String {
            let app = Router::new()
                .route("/mcp", post(mcp))
                .route("/token", post(token))
                .with_state(mock);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            format!("http://{addr}")
        }

        pub(crate) fn oauth_server(url: &str) -> McpServerConfig {
            McpServerConfig::http("secured", url).with_auth(McpAuth::OAuth {
                client_id: None,
                scopes: vec![],
                redirect_port: None,
                strict_scopes: false,
            })
        }

        pub(crate) async fn seed(
            backend: &Arc<MemoryBackend>,
            url: &str,
            credentials: StoredCredentials,
        ) {
            SecretStoreCredentialStore::with_backend(
                "secured",
                url,
                crate::mcp_auth::redirect_uri(None),
                backend.clone(),
            )
            .save(credentials)
            .await
            .unwrap();
        }

        pub(crate) async fn connect(
            server: &McpServerConfig,
            backend: &Arc<MemoryBackend>,
            tool_state: Arc<McpToolState>,
        ) -> Result<
            HashMap<String, RunningService<RoleClient, crate::elicitation::McpClientHandler>>,
            Error,
        > {
            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let sink = Arc::new(crate::event_sink::EventSink::new(
                storage.event_journal(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            ));
            let ctx = McpConnectContext::from_config(std::slice::from_ref(server))
                .with_secret_backend(backend.clone());
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                build_mcp_state(
                    &[server.to_acp()],
                    &ctx,
                    Default::default(),
                    sink,
                    "s1".into(),
                    &crate::agent::mcp::agent_implementation(),
                    tool_state,
                    None,
                ),
            )
            .await
            .expect("no hang")
        }

        /// Connect one server without `build_mcp_state`'s auth-required skip,
        /// exposing the raw `McpAuthRequired` error.
        async fn start_one(
            server: &McpServerConfig,
            backend: &Arc<MemoryBackend>,
        ) -> Result<(), Error> {
            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let sink = Arc::new(crate::event_sink::EventSink::new(
                storage.event_journal(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            ));
            let ctx = McpConnectContext::from_config(std::slice::from_ref(server))
                .with_secret_backend(backend.clone());
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                start_mcp_server(
                    &server.to_acp(),
                    &ctx,
                    Default::default(),
                    sink,
                    "s1".into(),
                    &crate::agent::mcp::agent_implementation(),
                    McpToolState::empty(),
                    None,
                ),
            )
            .await
            .expect("no hang")
            .map(|_| ())
        }

        #[tokio::test]
        async fn stored_valid_credentials_connect() {
            let mock = Arc::new(Mock::default());
            let url = format!("{}/mcp", serve(mock.clone()).await);
            let backend = Arc::new(MemoryBackend::default());
            seed(&backend, &url, creds("good-token")).await;

            let tool_state = McpToolState::empty();
            let _clients = connect(&oauth_server(&url), &backend, tool_state.clone())
                .await
                .expect("connects with stored credentials");

            assert!(tool_state.load().tools.contains_key("secret"));
            let bearers = mock.bearers.lock();
            assert!(!bearers.is_empty());
            assert!(
                bearers.iter().all(|b| b == "Bearer good-token"),
                "{bearers:?}"
            );
            assert_eq!(mock.refreshes.load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn missing_credentials_yield_auth_required() {
            let mock = Arc::new(Mock::default());
            let url = format!("{}/mcp", serve(mock.clone()).await);
            let backend = Arc::new(MemoryBackend::default());

            let Err(err) = start_one(&oauth_server(&url), &backend).await else {
                panic!("no credentials must not connect");
            };
            assert_eq!(
                crate::error::mcp_auth_required_server(&err),
                Some("secured"),
                "{err:?}"
            );
            assert!(
                mock.bearers.lock().is_empty(),
                "nothing is sent to the server without credentials"
            );
        }

        #[tokio::test]
        async fn auth_required_server_is_skipped_and_reported() {
            let mock = Arc::new(Mock::default());
            let url = format!("{}/mcp", serve(mock.clone()).await);
            let backend = Arc::new(MemoryBackend::default());

            // `secured` has no credentials; `plain` is a normal header server.
            let secured = oauth_server(&url);
            let plain = McpServerConfig::http("plain", &url).with_headers(
                [("Authorization".to_string(), "Bearer good-token".to_string())].into(),
            );
            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let sink = Arc::new(crate::event_sink::EventSink::new(
                storage.event_journal(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            ));
            let mut events = sink.fanout().subscribe();
            let ctx = McpConnectContext::from_config(std::slice::from_ref(&secured))
                .with_secret_backend(backend.clone());
            let tool_state = McpToolState::empty();
            let clients = build_mcp_state(
                &[secured.to_acp(), plain.to_acp()],
                &ctx,
                Default::default(),
                sink.clone(),
                "s1".into(),
                &crate::agent::mcp::agent_implementation(),
                tool_state.clone(),
                None,
            )
            .await
            .expect("session setup continues without the auth-required server");

            assert_eq!(clients.keys().collect::<Vec<_>>(), vec!["plain"]);
            assert!(tool_state.load().tools.contains_key("secret"));

            let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .expect("event published")
                .unwrap();
            assert_eq!(event.session_id(), "s1");
            assert!(matches!(
                event.kind(),
                crate::events::AgentEventKind::McpServerAuthRequired { server_name, provider_id, .. }
                    if server_name == "secured" && provider_id == "mcp:secured"
            ));
            assert!(event.is_ephemeral(), "transient");
        }

        #[tokio::test]
        async fn oauth_applied_servers_are_marked_on_the_tool_state() {
            let mock = Arc::new(Mock::default());
            let url = format!("{}/mcp", serve(mock.clone()).await);
            let backend = Arc::new(MemoryBackend::default());
            seed(&backend, &url, creds("good-token")).await;
            let tool_state = McpToolState::empty();
            let _clients = connect(&oauth_server(&url), &backend, tool_state.clone())
                .await
                .expect("connects with stored credentials");
            assert!(tool_state.is_oauth_server("secured"));
            assert!(!tool_state.is_oauth_server("other"));

            // Same name, different URL: connects without OAuth, so a 401 from
            // it must never be reported as "log in".
            let override_state = McpToolState::empty();
            let plain = McpServerConfig::http("secured", format!("{url}?mirror=1")).with_headers(
                [("Authorization".to_string(), "Bearer good-token".to_string())].into(),
            );
            let ctx = McpConnectContext::from_config(std::slice::from_ref(&oauth_server(&url)))
                .with_secret_backend(backend.clone());
            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let sink = Arc::new(crate::event_sink::EventSink::new(
                storage.event_journal(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            ));
            let _clients = build_mcp_state(
                &[plain.to_acp()],
                &ctx,
                Default::default(),
                sink,
                "s1".into(),
                &crate::agent::mcp::agent_implementation(),
                override_state.clone(),
                None,
            )
            .await
            .expect("override connects");
            assert!(!override_state.is_oauth_server("secured"));
        }

        #[tokio::test]
        async fn unreachable_authorization_skips_the_server_and_surfaces_the_error() {
            // A server whose authorization check fails is skipped (the
            // session still starts) and the failure is reported, not hidden.
            let backend = Arc::new(MemoryBackend::default());
            let bad_url = "http://127.0.0.1:1/mcp";
            seed(&backend, bad_url, creds("good-token")).await;
            let secured = oauth_server(bad_url);
            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let sink = Arc::new(crate::event_sink::EventSink::new(
                storage.event_journal(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            ));
            let mut events = sink.fanout().subscribe();
            let ctx = McpConnectContext::from_config(std::slice::from_ref(&secured))
                .with_secret_backend(backend.clone());
            let tool_state = McpToolState::empty();
            let clients = build_mcp_state(
                &[secured.to_acp()],
                &ctx,
                Default::default(),
                sink.clone(),
                "s1".into(),
                &crate::agent::mcp::agent_implementation(),
                tool_state.clone(),
                None,
            )
            .await
            .expect("an unreachable authorization server must not fail the session");
            assert!(clients.is_empty());

            let skipped = tool_state.skipped_servers();
            assert_eq!(skipped.len(), 1);
            assert_eq!(skipped[0].server_name, "secured");
            assert!(
                skipped[0].error.as_deref().is_some_and(|e| !e.is_empty()),
                "{skipped:?}"
            );
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .expect("event published")
                .unwrap();
            assert!(matches!(
                event.kind(),
                crate::events::AgentEventKind::McpServerAuthRequired { error: Some(_), .. }
            ));
        }

        #[tokio::test]
        async fn auth_required_server_is_recorded_without_an_error_text() {
            let backend = Arc::new(MemoryBackend::default());
            let secured = oauth_server("http://127.0.0.1:1/mcp");
            let tool_state = McpToolState::empty();
            // No credentials: skipped as "login required" without an error.
            let _ = connect(&secured, &backend, tool_state.clone()).await;
            let skipped = tool_state.skipped_servers();
            assert_eq!(skipped.len(), 1, "{skipped:?}");
            assert_eq!(skipped[0].provider_id, "mcp:secured");
            assert_eq!(skipped[0].error, None);
        }

        #[tokio::test]
        async fn broken_non_oauth_server_still_fails_the_session() {
            let backend = Arc::new(MemoryBackend::default());
            // Nothing listens on this port: a plain connection failure.
            let broken = McpServerConfig::http("broken", "http://127.0.0.1:1/mcp");
            let Err(err) = connect(&broken, &backend, McpToolState::empty()).await else {
                panic!("a broken non-OAuth server must fail session setup");
            };
            assert!(crate::error::mcp_auth_required_server(&err).is_none());
        }

        #[tokio::test]
        async fn rejected_refresh_during_a_call_reports_login_required() {
            use querymt::tool_decorator::CallFunctionTool;
            let mock = Arc::new(Mock::default());
            let url = format!("{}/mcp", serve(mock.clone()).await);
            let backend = Arc::new(MemoryBackend::default());
            seed(&backend, &url, creds("good-token")).await;

            let server = oauth_server(&url);
            let tool_state = McpToolState::empty();
            let _clients = connect(&server, &backend, tool_state.clone())
                .await
                .expect("connects with stored credentials");
            let adapter = tool_state.load().tools.get("secret").cloned().unwrap();

            // The server revokes the token and the refresh token is rejected.
            mock.revoked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            mock.reject_refresh
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let err = adapter
                .call(serde_json::json!({}))
                .await
                .expect_err("the call must fail once authorization is gone");

            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let sink = Arc::new(crate::event_sink::EventSink::new(
                storage.event_journal(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            ));
            let mut events = sink.fanout().subscribe();

            // A non-OAuth server with the same failure is not reported as a login.
            assert!(
                crate::agent::mcp::report_auth_required(&err, "secured", false, &sink, "s1")
                    .is_none()
            );

            let text = crate::agent::mcp::report_auth_required(&err, "secured", true, &sink, "s1")
                .expect("auth-required failure is recognised");
            assert_eq!(
                text,
                "Error: authorization required for MCP server secured; \
                 log in with provider mcp:secured"
            );
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .expect("event published")
                .unwrap();
            assert!(matches!(
                event.kind(),
                crate::events::AgentEventKind::McpServerAuthRequired { server_name, provider_id, .. }
                    if server_name == "secured" && provider_id == "mcp:secured"
            ));

            // rmcp cleared the stored credentials when the refresh was rejected.
            assert!(backend.0.lock().get("mcp-oauth:secured").is_none());
            let store = SecretStoreCredentialStore::with_backend(
                "secured",
                &url,
                crate::mcp_auth::redirect_uri(None),
                backend.clone(),
            );
            assert!(store.load().await.unwrap().is_none());
            assert_eq!(mock.refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn ordinary_call_failures_are_not_login_required() {
            let err =
                anyhow::Error::new(querymt::mcp::adapter::McpToolError::Reported { parts: vec![] });
            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let sink = crate::event_sink::EventSink::new(
                storage.event_journal(),
                Arc::new(crate::event_fanout::EventFanout::new()),
            );
            assert!(
                crate::agent::mcp::report_auth_required(&err, "secured", true, &sink, "s1")
                    .is_none()
            );
        }

        #[tokio::test]
        async fn credentials_for_another_url_yield_auth_required() {
            let mock = Arc::new(Mock::default());
            let url = format!("{}/mcp", serve(mock.clone()).await);
            let backend = Arc::new(MemoryBackend::default());
            seed(&backend, "https://elsewhere.test/mcp", creds("good-token")).await;

            let Err(err) = start_one(&oauth_server(&url), &backend).await else {
                panic!("credentials bound to another URL must not be used");
            };
            assert_eq!(
                crate::error::mcp_auth_required_server(&err),
                Some("secured")
            );
            assert!(mock.bearers.lock().is_empty());
        }

        #[tokio::test]
        async fn expired_access_token_is_refreshed_and_persisted() {
            let mock = Arc::new(Mock::default());
            let url = format!("{}/mcp", serve(mock.clone()).await);
            let backend = Arc::new(MemoryBackend::default());
            // Received at the epoch with a 60 s lifetime: long expired.
            let token: rmcp::transport::auth::OAuthTokenResponse =
                serde_json::from_value(serde_json::json!({
                    "access_token": "stale-token", "token_type": "bearer",
                    "refresh_token": "refresh-secret", "expires_in": 60}))
                .unwrap();
            seed(
                &backend,
                &url,
                StoredCredentials::new("client-1".into(), Some(token), vec![], Some(0)),
            )
            .await;

            let tool_state = McpToolState::empty();
            let _clients = connect(&oauth_server(&url), &backend, tool_state.clone())
                .await
                .expect("refreshes and connects");

            assert!(tool_state.load().tools.contains_key("secret"));
            assert_eq!(mock.refreshes.load(std::sync::atomic::Ordering::SeqCst), 1);
            let bearers = mock.bearers.lock();
            assert!(
                bearers.iter().all(|b| b == "Bearer fresh-token"),
                "{bearers:?}"
            );
            drop(bearers);

            let persisted = backend.0.lock().get("mcp-oauth:secured").cloned().unwrap();
            assert!(persisted.contains("fresh-token"), "new token persisted");
            assert!(
                persisted.contains("rotated-refresh"),
                "rotated refresh persisted"
            );
            assert!(!persisted.contains("stale-token"));
        }
    }

    #[cfg(not(feature = "oauth"))]
    #[tokio::test]
    async fn oauth_without_the_feature_reports_it() {
        use crate::config::{McpAuth, McpServerConfig};
        use crate::session::backend::StorageBackend;
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let sink = Arc::new(crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(crate::event_fanout::EventFanout::new()),
        ));
        let server = McpServerConfig::http("secured", "https://secured.test/mcp").with_auth(
            McpAuth::OAuth {
                client_id: None,
                scopes: vec![],
                redirect_port: None,
                strict_scopes: false,
            },
        );
        let ctx = McpConnectContext::from_config(std::slice::from_ref(&server));
        let Err(err) = build_mcp_state(
            &[server.to_acp()],
            &ctx,
            Default::default(),
            sink,
            "s1".into(),
            &crate::agent::mcp::agent_implementation(),
            McpToolState::empty(),
            None,
        )
        .await
        else {
            panic!("must fail without the oauth feature");
        };
        assert!(
            format!("{err:?}").contains("OAuth support is not enabled in this build"),
            "{err:?}"
        );
    }

    #[test]
    fn unsupported_platform_error_names_the_server() {
        assert_eq!(
            unix_socket_unsupported("sock"),
            "Unix sockets are unsupported for MCP server sock on this platform"
        );
    }

    /// Minimal HTTP/1.1 MCP endpoint on a Unix socket; records request headers.
    #[cfg(unix)]
    async fn serve_unix_mcp(
        listener: tokio::net::UnixListener,
        seen: Arc<parking_lot::Mutex<Vec<(String, String)>>>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut len = 0usize;
                for line in head.lines().skip(1) {
                    if let Some((k, v)) = line.split_once(':') {
                        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                        if k == "content-length" {
                            len = v.parse().unwrap_or(0);
                        }
                        seen.lock().push((k, v));
                    }
                }
                while buf.len() < head_end + len {
                    let n = stream.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body: serde_json::Value =
                    serde_json::from_slice(&buf[head_end..]).unwrap_or_default();
                let (status, payload) = match body.get("method").and_then(|m| m.as_str()) {
                    Some("initialize") => (
                        "200 OK",
                        serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": {
                            "protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                            "serverInfo": {"name": "unix-test", "version": "1"}}}),
                    ),
                    Some("tools/list") => (
                        "200 OK",
                        serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
                            "result": {"tools": [{"name": "ping", "inputSchema": {"type": "object"}}]}}),
                    ),
                    Some(m) if m.starts_with("notifications/") => {
                        ("202 Accepted", serde_json::Value::Null)
                    }
                    _ => (
                        "200 OK",
                        serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": {}}),
                    ),
                };
                let text = if payload.is_null() {
                    String::new()
                } else {
                    payload.to_string()
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nmcp-session-id: s\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                    text.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_server_connects_over_unix_socket_with_host_and_headers() {
        use crate::config::McpServerConfig;
        use crate::session::backend::StorageBackend;
        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("mcp.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let server = tokio::spawn(serve_unix_mcp(listener, seen.clone()));

        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let sink = Arc::new(crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(crate::event_fanout::EventFanout::new()),
        ));
        let config = McpServerConfig::http("sock", "http://mcp.internal/mcp")
            .with_socket_path(&socket)
            .with_headers([("X-Test".to_string(), "v1".to_string())].into());
        let tool_state = McpToolState::empty();
        let _clients = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            build_mcp_state(
                &[config.to_acp()],
                &McpConnectContext::default(),
                Default::default(),
                sink,
                "s1".into(),
                &crate::agent::mcp::agent_implementation(),
                tool_state.clone(),
                None,
            ),
        )
        .await
        .expect("no hang")
        .expect("connects over the socket");
        server.abort();

        assert!(
            tool_state.load().tools.contains_key("ping"),
            "tools listed over the socket"
        );
        let seen = seen.lock();
        assert!(
            seen.contains(&("host".into(), "mcp.internal".into())),
            "{seen:?}"
        );
        assert!(seen.contains(&("x-test".into(), "v1".into())), "{seen:?}");
    }
}
