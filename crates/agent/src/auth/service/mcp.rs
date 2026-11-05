//! `mcp:<server>` logins for OAuth HTTP MCP servers.
//!
//! The same `_querymt/auth/*` methods that drive provider logins accept the
//! provider id `mcp:<server>` for servers declared with `auth = oauth` in the
//! agent configuration. Credentials land in the MCP credential store (bound to
//! the server URL), where `start_mcp_server` finds them for sessions that
//! connect afterwards.

use super::*;
use crate::config::{McpAuth, McpServerConfig};
use crate::mcp_auth::{self, SecretStoreCredentialStore};
use rmcp::transport::{
    AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
};

/// Pending MCP logins, keyed like provider flows so a flow is only reachable
/// by the owner that started it.
pub(super) type McpFlowMap = Arc<Mutex<HashMap<FlowIdentity, Arc<McpFlow>>>>;

/// A started login awaiting its authorization callback.
pub(super) struct McpFlow {
    session: AuthorizationSession,
    /// `state` of the authorization URL, for completing with a bare code.
    state: String,
    created_at: Instant,
}

/// An OAuth server of the agent configuration.
struct OAuthServer {
    name: String,
    url: String,
    client_id: Option<String>,
    scopes: Vec<String>,
    redirect_port: Option<u16>,
}

impl OAuthServer {
    fn redirect_port(&self) -> u16 {
        self.redirect_port
            .unwrap_or(mcp_auth::DEFAULT_MCP_OAUTH_REDIRECT_PORT)
    }
}

fn unknown_server(server: &str) -> String {
    format!("unknown OAuth MCP server: {server}")
}

impl OAuthService {
    fn mcp_servers(&self) -> Vec<OAuthServer> {
        self.config
            .mcp_servers
            .iter()
            .filter_map(|s| match s {
                McpServerConfig::Http {
                    name,
                    url,
                    auth:
                        Some(McpAuth::OAuth {
                            client_id,
                            scopes,
                            redirect_port,
                        }),
                    ..
                } => Some(OAuthServer {
                    name: name.clone(),
                    url: url.clone(),
                    client_id: client_id.clone(),
                    scopes: scopes.clone(),
                    redirect_port: *redirect_port,
                }),
                _ => None,
            })
            .collect()
    }

    fn mcp_server(&self, name: &str) -> Option<OAuthServer> {
        self.mcp_servers().into_iter().find(|s| s.name == name)
    }

    fn mcp_store(&self, server: &OAuthServer) -> SecretStoreCredentialStore {
        SecretStoreCredentialStore::with_backend(
            &server.name,
            &server.url,
            mcp_auth::redirect_uri(server.redirect_port),
            self.mcp_backend.clone(),
        )
    }

    /// One entry per OAuth MCP server (or the one named by `filter`):
    /// connected when credentials are stored for the configured URL,
    /// otherwise a login is required.
    pub(super) async fn mcp_auth_status(&self, filter: Option<&str>) -> Vec<AuthProviderStatus> {
        let mut out = Vec::new();
        for server in self.mcp_servers() {
            let provider = mcp_auth::provider_id(&server.name);
            if filter.is_some_and(|f| f != provider) {
                continue;
            }
            let connected = matches!(
                self.mcp_store(&server).load().await,
                Ok(Some(c)) if c.token_response.is_some()
            );
            out.push(AuthProviderStatus {
                display_name: format!("MCP: {}", server.name),
                provider,
                oauth_status: Some(if connected {
                    OAuthStatus::Connected
                } else {
                    OAuthStatus::NotAuthenticated
                }),
                has_stored_api_key: false,
                has_env_api_key: false,
                env_var_name: None,
                supports_oauth: true,
                preferred_method: None,
            });
        }
        out
    }

    pub(super) async fn start_mcp_flow(
        &self,
        owner: &str,
        server: &str,
    ) -> Result<StartFlowResult, String> {
        let cfg = self
            .mcp_server(server)
            .ok_or_else(|| unknown_server(server))?;
        let provider = mcp_auth::provider_id(&cfg.name);
        let fail = |e: &dyn std::fmt::Display| {
            format!("Failed to start OAuth flow for MCP server {server}: {e}")
        };
        let redirect_uri = mcp_auth::redirect_uri(cfg.redirect_port);
        let store = self.mcp_store(&cfg);

        // Reuse the dynamically registered client only while the redirect URI
        // it was registered with is the one in use; otherwise register again.
        let mut client_id = cfg.client_id.clone();
        if client_id.is_none()
            && store.stored_redirect_uri().await.ok().flatten().as_deref()
                == Some(redirect_uri.as_str())
        {
            client_id = store.load().await.ok().flatten().map(|c| c.client_id);
        }

        let mut manager = AuthorizationManager::new(cfg.url.as_str())
            .await
            .map_err(|e| fail(&e))?;
        manager.set_credential_store(store);
        let resolution = manager.resolve_metadata().await.map_err(|e| fail(&e))?;
        manager.set_metadata(resolution.metadata);

        let mut request = AuthorizationRequest::new(redirect_uri)
            .with_client_name("QueryMT")
            .with_scopes(cfg.scopes.clone());
        if let Some(id) = client_id {
            request = request.with_preregistered_client(id);
        }
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| fail(&e))?;
        let authorization_url = session.get_authorization_url().to_string();
        let state = url::Url::parse(&authorization_url)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
            })
            .ok_or_else(|| fail(&"the authorization URL carries no state"))?;

        let identity = FlowIdentity {
            flow_id: uuid::Uuid::now_v7().to_string(),
            owner: owner.to_string(),
            provider: provider.clone(),
        };
        {
            let mut map = self.mcp_flows.lock().await;
            map.retain(|_, f| f.created_at.elapsed().as_secs() <= OAUTH_FLOW_TTL_SECS);
            map.insert(
                identity.clone(),
                Arc::new(McpFlow {
                    session,
                    state: state.clone(),
                    created_at: Instant::now(),
                }),
            );
        }
        let flow_id = identity.flow_id.clone();
        self.spawn_mcp_listener(identity, cfg.redirect_port(), state)
            .await;

        Ok(StartFlowResult {
            flow_id,
            provider,
            authorization_url,
            flow_kind: "redirect_code".to_string(),
        })
    }

    /// Serve the loopback redirect for `identity`, completing the login when
    /// the callback arrives. Shares the single listener slot with provider
    /// logins, so it replaces any pending listener the same way they do.
    async fn spawn_mcp_listener(&self, identity: FlowIdentity, port: u16, state: String) {
        stop_listener(&self.listener_slot, &self.flows, true).await;
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let svc = self.clone();
        let id = identity.clone();
        let task = tokio::spawn(async move {
            let Some((code, cb_state)) =
                await_loopback_callback(&id.flow_id, &id.provider, port, state, &mut stop_rx).await
            else {
                return;
            };
            let flow = svc.mcp_flows.lock().await.get(&id).cloned();
            let Some(flow) = flow else { return };
            match flow.session.handle_callback(&code, &cb_state).await {
                Ok(_) => {
                    svc.mcp_flows.lock().await.remove(&id);
                    let mut active = svc.listener_slot.lock().await;
                    if active.as_ref().is_some_and(|l| l.flow_id == id.flow_id) {
                        active.take();
                    }
                    log::info!("MCP OAuth login completed for '{}'", id.provider);
                }
                Err(e) => log::warn!("MCP OAuth token exchange failed for '{}': {e}", id.provider),
            }
        });
        *self.listener_slot.lock().await = Some(ActiveOAuthCallbackListener {
            flow_id: identity.flow_id,
            owner: identity.owner,
            provider: identity.provider,
            stop_tx,
            task,
        });
    }

    /// Complete an `mcp:` login with a redirect URL or a bare code.
    ///
    /// Returns `None` when `owner` has no such MCP flow, so another owner's
    /// flow is indistinguishable from a nonexistent one.
    pub(super) async fn complete_mcp_flow(
        &self,
        owner: &str,
        flow_id: &str,
        response: &str,
    ) -> Option<CompleteFlowResult> {
        let (identity, flow) = {
            let map = self.mcp_flows.lock().await;
            map.iter()
                .find(|(k, _)| k.flow_id == flow_id && k.owner == owner)
                .map(|(k, v)| (k.clone(), v.clone()))?
        };
        let result = |success: bool, message: String| CompleteFlowResult {
            provider: identity.provider.clone(),
            success,
            message,
        };
        if flow.created_at.elapsed().as_secs() > OAUTH_FLOW_TTL_SECS {
            self.mcp_flows.lock().await.remove(&identity);
            stop_listener_for_flow(&self.listener_slot, &self.flows, flow_id, false).await;
            return Some(result(
                false,
                "OAuth flow expired, please start again".to_string(),
            ));
        }
        let input = response.trim();
        if input.is_empty() {
            return Some(result(
                false,
                "Authorization response is required".to_string(),
            ));
        }
        let exchanged = if input.starts_with("http://") || input.starts_with("https://") {
            flow.session.handle_callback_url(input).await
        } else {
            let code = input.split('#').next().unwrap_or(input);
            flow.session.handle_callback(code, &flow.state).await
        };
        Some(match exchanged {
            Ok(_) => {
                self.mcp_flows.lock().await.remove(&identity);
                stop_listener_for_flow(&self.listener_slot, &self.flows, flow_id, false).await;
                result(
                    true,
                    format!("Successfully authenticated with {}", identity.provider),
                )
            }
            Err(e) => result(false, format!("Token exchange failed: {e}")),
        })
    }

    pub(super) async fn logout_mcp(&self, owner: &str, server: &str) -> LogoutResult {
        let provider = mcp_auth::provider_id(server);
        let Some(cfg) = self.mcp_server(server) else {
            return LogoutResult {
                provider,
                success: false,
                message: unknown_server(server),
            };
        };
        match self.mcp_store(&cfg).clear().await {
            Ok(()) => {
                self.mcp_flows
                    .lock()
                    .await
                    .retain(|k, _| !(k.owner == owner && k.provider == provider));
                stop_listener_for_owner_provider(&self.listener_slot, owner, &provider).await;
                LogoutResult {
                    provider,
                    success: true,
                    message: format!("Disconnected from MCP server {server}"),
                }
            }
            Err(e) => LogoutResult {
                provider,
                success: false,
                message: format!("Failed to remove credentials: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_auth::test_support::MemoryBackend;
    use axum::{
        Json, Router,
        extract::State,
        routing::{get, post},
    };

    #[derive(Default)]
    struct AuthServer {
        base: String,
        registrations: parking_lot::Mutex<Vec<serde_json::Value>>,
    }

    /// Minimal authorization server: metadata, dynamic registration, tokens.
    async fn serve_auth_server() -> Arc<AuthServer> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(AuthServer {
            base,
            ..Default::default()
        });
        async fn metadata(State(s): State<Arc<AuthServer>>) -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "issuer": s.base,
                "authorization_endpoint": format!("{}/authorize", s.base),
                "token_endpoint": format!("{}/token", s.base),
                "registration_endpoint": format!("{}/register", s.base),
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
            }))
        }
        async fn register(
            State(s): State<Arc<AuthServer>>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            let redirect_uris = body["redirect_uris"].clone();
            s.registrations.lock().push(body);
            Json(serde_json::json!({"client_id": "dcr-client", "redirect_uris": redirect_uris}))
        }
        async fn token() -> Json<serde_json::Value> {
            Json(serde_json::json!({
                "access_token": "login-access", "token_type": "bearer",
                "refresh_token": "login-refresh", "expires_in": 3600
            }))
        }
        // The protected MCP endpoint of the same host: accepts only the token
        // that `/token` hands out.
        async fn mcp(
            headers: axum::http::HeaderMap,
            body: axum::body::Bytes,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let bearer = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if bearer != "Bearer login-access" {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [("www-authenticate", "Bearer")],
                )
                    .into_response();
            }
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let payload = match body.get("method").and_then(|m| m.as_str()) {
                Some("initialize") => serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
                    "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}},
                        "serverInfo": {"name": "login-test", "version": "1"}}}),
                Some("tools/list") => serde_json::json!({"jsonrpc": "2.0", "id": body["id"],
                    "result": {"tools": [{"name": "issues", "inputSchema": {"type": "object"}}]}}),
                Some(m) if m.starts_with("notifications/") => {
                    return axum::http::StatusCode::ACCEPTED.into_response();
                }
                _ => serde_json::json!({"jsonrpc": "2.0", "id": body["id"], "result": {}}),
            };
            Json(payload).into_response()
        }
        let app = Router::new()
            .route("/.well-known/oauth-authorization-server", get(metadata))
            .route("/register", post(register))
            .route("/token", post(token))
            .route("/mcp", post(mcp))
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        state
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn service(
        server: &AuthServer,
        backend: &Arc<MemoryBackend>,
        port: u16,
    ) -> (OAuthService, String) {
        let fixture = crate::test_utils::TestAgent::new().await;
        let url = format!("{}/mcp", server.base);
        let mut config = (*fixture.config).clone();
        config.mcp_servers = vec![
            McpServerConfig::http("linear", &url).with_auth(McpAuth::OAuth {
                client_id: None,
                scopes: vec!["read".into()],
                redirect_port: Some(port),
            }),
            McpServerConfig::http("plain", "https://plain.test/mcp"),
        ];
        let config = Arc::new(config);
        let svc = OAuthService::new(
            config.clone(),
            ModelInventory::new(config),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(None)),
        )
        .with_mcp_secret_backend(backend.clone());
        (svc, url)
    }

    fn state_of(url: &str) -> String {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned()
    }

    async fn hit_callback(port: u16, query: &str) {
        for _ in 0..100 {
            if let Ok(r) = reqwest::get(format!("http://127.0.0.1:{port}/callback?{query}")).await
                && r.status().is_success()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        panic!("loopback listener never answered");
    }

    async fn wait_for_credentials(backend: &MemoryBackend) -> String {
        for _ in 0..100 {
            if let Some(v) = backend.0.lock().get("mcp-oauth:linear").cloned() {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        panic!("credentials were never stored");
    }

    fn status_of(statuses: &[AuthProviderStatus], provider: &str) -> Option<OAuthStatus> {
        statuses
            .iter()
            .find(|s| s.provider == provider)
            .and_then(|s| s.oauth_status.clone())
    }

    #[tokio::test]
    async fn unknown_or_non_oauth_server_is_rejected() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;
        for name in ["unknown", "plain"] {
            let err = svc
                .start_flow("acp", &format!("mcp:{name}"))
                .await
                .unwrap_err();
            assert_eq!(err, format!("unknown OAuth MCP server: {name}"));
            let out = svc.logout("acp", &format!("mcp:{name}")).await;
            assert!(!out.success);
            assert_eq!(out.message, format!("unknown OAuth MCP server: {name}"));
        }
    }

    #[tokio::test]
    async fn status_lists_one_entry_per_oauth_server() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;
        let all = svc.mcp_auth_status(None).await;
        assert_eq!(all.len(), 1, "only the OAuth server is listed");
        assert_eq!(
            status_of(&all, "mcp:linear"),
            Some(OAuthStatus::NotAuthenticated)
        );
        assert!(svc.mcp_auth_status(Some("mcp:plain")).await.is_empty());
    }

    #[tokio::test]
    async fn loopback_login_stores_credentials_and_reports_connected() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let port = free_port();
        let (svc, url) = service(&auth, &backend, port).await;

        let started = svc.start_flow("acp", "mcp:linear").await.unwrap();
        assert_eq!(started.provider, "mcp:linear");
        assert_eq!(started.flow_kind, "redirect_code");
        assert!(started.authorization_url.starts_with(&auth.base));
        {
            let regs = auth.registrations.lock();
            assert_eq!(regs.len(), 1);
            assert_eq!(
                regs[0]["redirect_uris"],
                serde_json::json!([format!("http://127.0.0.1:{port}/callback")])
            );
        }

        hit_callback(
            port,
            &format!("code=abc&state={}", state_of(&started.authorization_url)),
        )
        .await;
        let stored = wait_for_credentials(&backend).await;
        assert!(stored.contains("login-access"));
        assert!(stored.contains(&url), "bound to the server URL");
        assert!(stored.contains(&format!("http://127.0.0.1:{port}/callback")));

        let statuses = svc.auth_status(Some("mcp:linear")).await.unwrap();
        assert_eq!(
            status_of(&statuses, "mcp:linear"),
            Some(OAuthStatus::Connected)
        );
    }

    #[tokio::test]
    async fn another_owner_cannot_complete_the_flow() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;
        let started = svc.start_flow("owner-a", "mcp:linear").await.unwrap();
        let state = state_of(&started.authorization_url);

        let other = svc
            .complete_flow("owner-b", &started.flow_id, &format!("abc#{state}"))
            .await;
        assert!(!other.success);
        assert!(other.message.contains("Unknown OAuth flow"), "{other:?}");
        assert!(backend.0.lock().is_empty(), "nothing persisted");

        // The owner can still finish it by pasting the redirect URL.
        let redirect = format!("http://127.0.0.1:1/callback?code=abc&state={state}");
        let done = svc
            .complete_flow("owner-a", &started.flow_id, &redirect)
            .await;
        assert!(done.success, "{done:?}");
        assert_eq!(done.provider, "mcp:linear");
        assert!(backend.0.lock().contains_key("mcp-oauth:linear"));

        // A completed flow is gone.
        let again = svc
            .complete_flow("owner-a", &started.flow_id, &redirect)
            .await;
        assert!(!again.success);
    }

    #[tokio::test]
    async fn redirect_uri_is_stable_across_logins() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let port = free_port();
        let (svc, _) = service(&auth, &backend, port).await;
        svc.start_flow("acp", "mcp:linear").await.unwrap();
        svc.start_flow("acp", "mcp:linear").await.unwrap();
        let regs = auth.registrations.lock();
        assert_eq!(regs.len(), 2, "no credentials yet: each login registers");
        assert_eq!(regs[0]["redirect_uris"], regs[1]["redirect_uris"]);
        assert_eq!(
            mcp_auth::redirect_uri(None),
            format!(
                "http://127.0.0.1:{}/callback",
                mcp_auth::DEFAULT_MCP_OAUTH_REDIRECT_PORT
            ),
            "default when no port is configured"
        );
    }

    /// Connect the agent's MCP servers the way a new session does.
    async fn connect_session(
        svc: &OAuthService,
        backend: &Arc<MemoryBackend>,
        session_id: &str,
    ) -> (
        Arc<crate::agent::core::McpToolState>,
        HashMap<
            String,
            rmcp::service::RunningService<rmcp::RoleClient, crate::elicitation::McpClientHandler>,
        >,
    ) {
        use crate::session::backend::StorageBackend;
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let sink = Arc::new(crate::event_sink::EventSink::new(
            storage.event_journal(),
            Arc::new(crate::event_fanout::EventFanout::new()),
        ));
        let ctx = crate::agent::mcp::McpConnectContext::from_config(&svc.config.mcp_servers)
            .with_secret_backend(backend.clone());
        // Only the OAuth server: the other configured one is unreachable.
        let servers: Vec<_> = svc
            .config
            .mcp_servers
            .iter()
            .filter(|s| s.name() == "linear")
            .map(|s| s.to_acp())
            .collect();
        let tool_state = crate::agent::core::McpToolState::empty();
        let clients = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::agent::protocol::build_mcp_state(
                &servers,
                &ctx,
                Default::default(),
                sink,
                session_id.into(),
                &crate::agent::mcp::agent_implementation(),
                tool_state.clone(),
                None,
            ),
        )
        .await
        .expect("no hang")
        .expect("session setup succeeds");
        (tool_state, clients)
    }

    #[tokio::test]
    async fn login_makes_the_server_available_to_later_sessions_only() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;

        // Session A starts before any login: the server is skipped.
        let (tools_a, clients_a) = connect_session(&svc, &backend, "session-a").await;
        assert!(clients_a.is_empty());
        assert!(tools_a.load().tools.is_empty());

        // The login completes without restarting anything.
        let started = svc.start_flow("acp", "mcp:linear").await.unwrap();
        let state = state_of(&started.authorization_url);
        let done = svc
            .complete_flow("acp", &started.flow_id, &format!("abc#{state}"))
            .await;
        assert!(done.success, "{done:?}");

        // Session B connects the server and has its tools.
        let (tools_b, clients_b) = connect_session(&svc, &backend, "session-b").await;
        assert!(clients_b.contains_key("linear"));
        assert!(tools_b.load().tools.contains_key("issues"));

        // Session A keeps its existing connections.
        assert!(clients_a.is_empty());
        assert!(tools_a.load().tools.is_empty());
    }

    #[tokio::test]
    async fn logout_clears_credentials() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;
        let started = svc.start_flow("acp", "mcp:linear").await.unwrap();
        let state = state_of(&started.authorization_url);
        let done = svc
            .complete_flow("acp", &started.flow_id, &format!("abc#{state}"))
            .await;
        assert!(done.success, "{done:?}");
        assert!(backend.0.lock().contains_key("mcp-oauth:linear"));

        let out = svc.logout("acp", "mcp:linear").await;
        assert!(out.success, "{out:?}");
        assert!(backend.0.lock().is_empty());
        let statuses = svc.auth_status(Some("mcp:linear")).await.unwrap();
        assert_eq!(
            status_of(&statuses, "mcp:linear"),
            Some(OAuthStatus::NotAuthenticated)
        );
    }
}
