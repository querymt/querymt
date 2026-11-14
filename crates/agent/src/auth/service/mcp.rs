//! `mcp:<server>` logins for OAuth HTTP MCP servers.
//!
//! The same `_querymt/auth/*` methods that drive provider logins accept the
//! provider id `mcp:<server>` for servers declared with `auth = oauth`.
//! Servers are gathered from the agent configuration, every profile of the
//! profile catalog (including its `.agents/` MCP servers) and the running
//! profile runtimes, so a server that only one profile declares can still be
//! logged into before any of its sessions exists. Credentials land in the MCP
//! credential store (bound to the server URL), where `start_mcp_server` finds
//! them for sessions that connect afterwards.

use super::*;
use crate::config::{McpAuth, McpServerConfig};
use crate::mcp_auth::{self, SecretStoreCredentialStore};
use rmcp::transport::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
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

/// An OAuth MCP server declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OAuthServer {
    name: String,
    url: String,
    auth: McpAuth,
}

impl OAuthServer {
    fn from_config(server: &McpServerConfig) -> Option<Self> {
        match server {
            McpServerConfig::Http {
                name,
                url,
                auth: Some(auth),
                ..
            } => Some(Self {
                name: name.clone(),
                url: url.clone(),
                auth: auth.clone(),
            }),
            _ => None,
        }
    }

    /// Same server, settings and (normalised) URL.
    fn same_definition(&self, other: &Self) -> bool {
        self.name == other.name
            && self.auth == other.auth
            && mcp_auth::same_url(&self.url, &other.url)
    }

    fn client_id(&self) -> Option<&str> {
        let McpAuth::OAuth { client_id, .. } = &self.auth;
        client_id.as_deref()
    }

    fn scopes(&self) -> &[String] {
        let McpAuth::OAuth { scopes, .. } = &self.auth;
        scopes
    }

    fn configured_port(&self) -> Option<u16> {
        let McpAuth::OAuth { redirect_port, .. } = &self.auth;
        *redirect_port
    }

    fn redirect_port(&self) -> u16 {
        self.configured_port()
            .unwrap_or(mcp_auth::DEFAULT_MCP_OAUTH_REDIRECT_PORT)
    }

    fn redirect_uri(&self) -> String {
        mcp_auth::redirect_uri(self.configured_port())
    }
}

/// Every declaration of one server name, merged.
struct DeclaredServer {
    /// The first declaration seen.
    server: OAuthServer,
    /// Where it is declared (agent configuration, profiles, `.agents/`).
    sources: Vec<String>,
    /// Some declaration differs from `server`.
    conflicting: bool,
}

/// Label of servers declared by the agent configuration itself.
const AGENT_CONFIG_SOURCE: &str = "agent configuration";

/// Span status, `error.type` and the login counter of MCP auth operations.
mod telemetry {
    use super::AuthFlowError;
    use opentelemetry::trace::Status;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    /// Record how `operation` (`start`, `callback`, `logout`) ended on `span`
    /// and count it in `querymt.auth.mcp.login{operation, outcome, error.type}`.
    pub(super) fn record_outcome(
        span: &tracing::Span,
        operation: &'static str,
        error: Option<&AuthFlowError>,
    ) {
        record(span, operation, error.map(AuthFlowError::error_type));
    }

    /// Same, for failures that are not an [`AuthFlowError`].
    pub(super) fn record(
        span: &tracing::Span,
        operation: &'static str,
        error_type: Option<&'static str>,
    ) {
        let mut attributes = vec![
            opentelemetry::KeyValue::new("operation", operation),
            opentelemetry::KeyValue::new(
                "outcome",
                if error_type.is_some() { "error" } else { "ok" },
            ),
        ];
        if let Some(error_type) = error_type {
            span.set_attribute("error.type", error_type);
            span.set_status(Status::error(""));
            attributes.push(opentelemetry::KeyValue::new("error.type", error_type));
        }
        opentelemetry::global::meter("querymt-agent")
            .u64_counter("querymt.auth.mcp.login")
            .with_description("MCP OAuth login operations by outcome")
            .build()
            .add(1, &attributes);
    }
}

impl OAuthService {
    /// Every MCP server declaration the agent can start sessions with,
    /// labelled with its source.
    async fn mcp_declarations(&self) -> Vec<(String, McpServerConfig)> {
        let mut declarations: Vec<_> = self
            .config
            .mcp_servers
            .iter()
            .map(|server| (AGENT_CONFIG_SOURCE.to_string(), server.clone()))
            .collect();
        // Clone the manager out of the slot before awaiting on it.
        let profiles = self.profiles.load_full().as_ref().clone();
        if let Some(profiles) = profiles {
            declarations.extend(profiles.mcp_servers().await);
        }
        declarations
    }

    /// OAuth servers by name, in first-seen order. Identical declarations
    /// from several sources are merged; differing ones are flagged.
    async fn oauth_servers(&self) -> Vec<DeclaredServer> {
        let mut servers: Vec<DeclaredServer> = Vec::new();
        for (source, config) in self.mcp_declarations().await {
            let Some(server) = OAuthServer::from_config(&config) else {
                continue;
            };
            match servers.iter_mut().find(|d| d.server.name == server.name) {
                Some(declared) => {
                    declared.conflicting |= !declared.server.same_definition(&server);
                    if !declared.sources.contains(&source) {
                        declared.sources.push(source);
                    }
                }
                None => servers.push(DeclaredServer {
                    server,
                    sources: vec![source],
                    conflicting: false,
                }),
            }
        }
        servers
    }

    /// The OAuth server named `name`.
    ///
    /// # Errors
    /// [`AuthFlowError::UnknownMcpServer`] when no source declares it with
    /// OAuth, [`AuthFlowError::ConflictingMcpServer`] when sources disagree:
    /// credentials are stored per name, so one login cannot serve both.
    async fn mcp_server(&self, name: &str) -> Result<OAuthServer, AuthFlowError> {
        let declared = self
            .oauth_servers()
            .await
            .into_iter()
            .find(|d| d.server.name == name)
            .ok_or_else(|| AuthFlowError::UnknownMcpServer(name.to_string()))?;
        if declared.conflicting {
            return Err(AuthFlowError::ConflictingMcpServer {
                server: name.to_string(),
                sources: declared.sources,
            });
        }
        Ok(declared.server)
    }

    fn mcp_store(&self, server: &OAuthServer) -> SecretStoreCredentialStore {
        SecretStoreCredentialStore::with_backend(
            &server.name,
            &server.url,
            server.redirect_uri(),
            self.mcp_backend.clone(),
        )
        .with_auth_policy(&server.auth)
    }

    /// One entry per OAuth MCP server (or the one named by `filter`):
    /// connected when usable credentials are stored for the configured URL,
    /// otherwise a login is required.
    pub(super) async fn mcp_auth_status(&self, filter: Option<&str>) -> Vec<AuthProviderStatus> {
        let mut out = Vec::new();
        for DeclaredServer { server, .. } in self.oauth_servers().await {
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

    /// Begin an `mcp:` login.
    ///
    /// Traced as `auth.mcp.start` with child spans for discovery and
    /// registration; a failure marks the span (`error.type`) and counts in
    /// `querymt.auth.mcp.login`. Logging is left to the handler boundary.
    pub(super) async fn start_mcp_flow(
        &self,
        owner: &str,
        server: &str,
    ) -> Result<StartFlowResult, AuthFlowError> {
        use tracing::Instrument as _;
        let span = tracing::info_span!(
            "auth.mcp.start",
            provider_id = %mcp_auth::provider_id(server),
            server,
            sources = tracing::field::Empty,
            scopes_requested = tracing::field::Empty,
        );
        let result = self
            .start_mcp_flow_inner(owner, server, &span)
            .instrument(span.clone())
            .await;
        telemetry::record_outcome(&span, "start", result.as_ref().err());
        result
    }

    async fn start_mcp_flow_inner(
        &self,
        owner: &str,
        server: &str,
        span: &tracing::Span,
    ) -> Result<StartFlowResult, AuthFlowError> {
        use tracing::Instrument as _;
        let declared = self
            .oauth_servers()
            .await
            .into_iter()
            .find(|d| d.server.name == server)
            .ok_or_else(|| AuthFlowError::UnknownMcpServer(server.to_string()))?;
        span.record("sources", declared.sources.join(", ").as_str());
        if declared.conflicting {
            return Err(AuthFlowError::ConflictingMcpServer {
                server: server.to_string(),
                sources: declared.sources,
            });
        }
        let cfg = declared.server;
        span.record("scopes_requested", cfg.scopes().len() as u64);
        let provider = mcp_auth::provider_id(&cfg.name);
        let fail = |step: &'static str| {
            move |source: AuthError| AuthFlowError::McpAuthorization {
                server: server.to_string(),
                step,
                source: Box::new(source),
            }
        };
        let redirect_uri = cfg.redirect_uri();
        let store = self.mcp_store(&cfg);

        // Reuse the dynamically registered client only while the redirect URI
        // it was registered with is the one in use; otherwise register again.
        let mut client_id = cfg.client_id().map(str::to_owned);
        if client_id.is_none()
            && store.stored_redirect_uri().await.ok().flatten().as_deref()
                == Some(redirect_uri.as_str())
        {
            client_id = store.load().await.ok().flatten().map(|c| c.client_id);
        }

        let http = mcp_auth::oauth_http_client().map_err(fail("http client"))?;
        let mut manager = AuthorizationManager::new_with_oauth_http_client(cfg.url.as_str(), http)
            .await
            .map_err(fail("metadata discovery"))?;
        manager.set_credential_store(store);
        let resolution = manager
            .resolve_metadata()
            .instrument(tracing::info_span!("auth.mcp.discover", server))
            .await
            .map_err(fail("metadata discovery"))?;
        manager.set_metadata(resolution.metadata);

        let mut request = AuthorizationRequest::new(redirect_uri)
            .with_client_name("QueryMT")
            .with_scopes(cfg.scopes().to_vec());
        let preregistered = client_id.is_some();
        if let Some(id) = client_id {
            request = request.with_preregistered_client(id);
        }
        let session = AuthorizationSession::new(manager, request)
            .instrument(tracing::info_span!(
                "auth.mcp.register",
                server,
                preregistered
            ))
            .await
            .map_err(|(_, e)| fail("client registration")(e))?;
        let authorization_url = session.get_authorization_url().to_string();
        let state = url::Url::parse(&authorization_url)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
            })
            .ok_or_else(|| {
                fail("authorization request")(AuthError::InternalError(
                    "the authorization URL carries no state".to_string(),
                ))
            })?;

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
        use tracing::Instrument as _;
        stop_listener(&self.listener_slot, &self.flows, true).await;
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        let svc = self.clone();
        let id = identity.clone();
        let task = tokio::spawn(async move {
            let Some(callback) =
                await_loopback_callback(&id.flow_id, &id.provider, port, state, &mut stop_rx).await
            else {
                return;
            };
            let flow = svc.mcp_flows.lock().await.get(&id).cloned();
            let Some(flow) = flow else { return };
            // Hand over the RFC 9207 `iss` of the redirect: servers that
            // advertise `authorization_response_iss_parameter_supported`
            // (Linear) are refused without it.
            let span = tracing::info_span!(
                "auth.mcp.callback",
                provider_id = %id.provider,
                channel = "loopback"
            );
            let exchanged = flow
                .session
                .handle_callback_with_issuer(
                    &callback.code,
                    &callback.state,
                    callback.issuer.as_deref(),
                )
                .instrument(span.clone())
                .await;
            match exchanged {
                Ok(_) => {
                    telemetry::record(&span, "callback", None);
                    svc.mcp_flows.lock().await.remove(&id);
                    let mut active = svc.listener_slot.lock().await;
                    if active.as_ref().is_some_and(|l| l.flow_id == id.flow_id) {
                        active.take();
                    }
                    tracing::info!(provider_id = %id.provider, "MCP OAuth login completed");
                }
                Err(e) => {
                    let error_type = mcp_auth::auth_error_type(&e);
                    telemetry::record(&span, "callback", Some(error_type));
                    tracing::error!(
                        provider_id = %id.provider,
                        error_type,
                        error = %e,
                        "MCP OAuth token exchange failed"
                    );
                }
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
        use tracing::Instrument as _;
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
        let span = tracing::info_span!(
            "auth.mcp.callback",
            provider_id = %identity.provider,
            channel = "paste"
        );
        let exchanged = async {
            if input.starts_with("http://") || input.starts_with("https://") {
                flow.session.handle_callback_url(input).await
            } else {
                // A bare code carries no `iss`; servers that require it reject
                // this, and the error then asks for the full callback URL.
                let code = input.split('#').next().unwrap_or(input);
                flow.session.handle_callback(code, &flow.state).await
            }
        }
        .instrument(span.clone())
        .await;
        Some(match exchanged {
            Ok(_) => {
                telemetry::record(&span, "callback", None);
                self.mcp_flows.lock().await.remove(&identity);
                stop_listener_for_flow(&self.listener_slot, &self.flows, flow_id, false).await;
                tracing::info!(provider_id = %identity.provider, "MCP OAuth login completed");
                result(
                    true,
                    format!("Successfully authenticated with {}", identity.provider),
                )
            }
            Err(rmcp::transport::AuthError::AuthorizationServerMissingIssuer { .. })
                if !input.starts_with("http") =>
            {
                // The user pasted a bare code: ask again, not a server fault.
                tracing::warn!(
                    provider_id = %identity.provider,
                    "pasted code lacks the `iss` this server requires"
                );
                result(
                    false,
                    "This server needs the full callback URL (it includes the `iss` parameter); paste the whole URL, not just the code"
                        .to_string(),
                )
            }
            Err(e) => {
                let error_type = mcp_auth::auth_error_type(&e);
                telemetry::record(&span, "callback", Some(error_type));
                tracing::error!(
                    provider_id = %identity.provider,
                    error_type,
                    error = %e,
                    "MCP OAuth token exchange failed"
                );
                result(false, format!("Token exchange failed: {e}"))
            }
        })
    }

    pub(super) async fn logout_mcp(&self, owner: &str, server: &str) -> LogoutResult {
        use tracing::Instrument as _;
        let provider = mcp_auth::provider_id(server);
        let span = tracing::info_span!("auth.mcp.logout", provider_id = %provider, server);
        async {
            let cfg = match self.mcp_server(server).await {
                Ok(cfg) => cfg,
                Err(error) => {
                    error.log("logout", &provider);
                    telemetry::record_outcome(&tracing::Span::current(), "logout", Some(&error));
                    return LogoutResult {
                        provider: provider.clone(),
                        success: false,
                        message: error.to_string(),
                    };
                }
            };
            match self.mcp_store(&cfg).clear().await {
                Ok(()) => {
                    self.mcp_flows
                        .lock()
                        .await
                        .retain(|k, _| !(k.owner == owner && k.provider == provider));
                    stop_listener_for_owner_provider(&self.listener_slot, owner, &provider).await;
                    telemetry::record(&tracing::Span::current(), "logout", None);
                    LogoutResult {
                        provider: provider.clone(),
                        success: true,
                        message: format!("Disconnected from MCP server {server}"),
                    }
                }
                Err(e) => {
                    let error_type = mcp_auth::auth_error_type(&e);
                    telemetry::record(&tracing::Span::current(), "logout", Some(error_type));
                    tracing::error!(provider_id = %provider, error_type, error = %e, "removing MCP credentials failed");
                    LogoutResult {
                        provider: provider.clone(),
                        success: false,
                        message: format!("Failed to remove credentials: {e}"),
                    }
                }
            }
        }
        .instrument(span)
        .await
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
        /// `scope` of token responses; omitted when `None`.
        token_scope: parking_lot::Mutex<Option<String>>,
        /// Refuse every OAuth request with 403.
        refuse: std::sync::atomic::AtomicBool,
    }

    /// Like IBKR: OAuth endpoints answer 403 to requests without a
    /// `User-Agent` (or while `refuse` is set). The MCP endpoint is exempt.
    async fn gatekeeper(
        State(s): State<Arc<AuthServer>>,
        request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let exempt = request.uri().path() == "/mcp";
        let has_agent = request.headers().contains_key("user-agent");
        if !exempt && (!has_agent || s.refuse.load(std::sync::atomic::Ordering::SeqCst)) {
            return axum::http::StatusCode::FORBIDDEN.into_response();
        }
        next.run(request).await
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
                // Like Linear: the redirect must carry `iss` (RFC 9207).
                "authorization_response_iss_parameter_supported": true,
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
        async fn token(State(s): State<Arc<AuthServer>>) -> Json<serde_json::Value> {
            let mut body = serde_json::json!({
                "access_token": "login-access", "token_type": "bearer",
                "refresh_token": "login-refresh", "expires_in": 3600
            });
            if let Some(scope) = s.token_scope.lock().clone() {
                body["scope"] = scope.into();
            }
            Json(body)
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
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                gatekeeper,
            ))
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
                strict_scopes: false,
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

    /// Query string of a redirect as the authorization server sends it: with
    /// the RFC 9207 `iss` of the server.
    fn redirect_query(auth: &AuthServer, authorization_url: &str) -> String {
        let iss: String = url::form_urlencoded::byte_serialize(auth.base.as_bytes()).collect();
        format!("code=abc&state={}&iss={iss}", state_of(authorization_url))
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
            assert!(
                matches!(&err, AuthFlowError::UnknownMcpServer(n) if n == name),
                "{err:?}"
            );
            assert_eq!(err.to_string(), format!("unknown OAuth MCP server: {name}"));
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

        hit_callback(port, &redirect_query(&auth, &started.authorization_url)).await;
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
    async fn loopback_redirect_without_issuer_is_refused() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let port = free_port();
        let (svc, _) = service(&auth, &backend, port).await;

        let started = svc.start_flow("acp", "mcp:linear").await.unwrap();
        // The server requires `iss`; a redirect without it must not log in.
        hit_callback(
            port,
            &format!("code=abc&state={}", state_of(&started.authorization_url)),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(backend.0.lock().is_empty(), "nothing persisted");
        let statuses = svc.auth_status(Some("mcp:linear")).await.unwrap();
        assert_eq!(
            status_of(&statuses, "mcp:linear"),
            Some(OAuthStatus::NotAuthenticated)
        );
    }

    #[tokio::test]
    async fn loopback_redirect_with_a_foreign_issuer_is_refused() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let port = free_port();
        let (svc, _) = service(&auth, &backend, port).await;

        let started = svc.start_flow("acp", "mcp:linear").await.unwrap();
        hit_callback(
            port,
            &format!(
                "code=abc&state={}&iss=https%3A%2F%2Fevil.example",
                state_of(&started.authorization_url)
            ),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(backend.0.lock().is_empty(), "nothing persisted");
    }

    #[tokio::test]
    async fn pasting_a_bare_code_explains_that_the_full_url_is_needed() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;
        let started = svc.start_flow("acp", "mcp:linear").await.unwrap();
        let state = state_of(&started.authorization_url);

        let bare = svc
            .complete_flow("acp", &started.flow_id, &format!("abc#{state}"))
            .await;
        assert!(!bare.success);
        assert!(bare.message.contains("full callback URL"), "{bare:?}");
        assert!(backend.0.lock().is_empty());

        // The full redirect URL still completes the same flow.
        let redirect = format!(
            "http://127.0.0.1:1/callback?{}",
            redirect_query(&auth, &started.authorization_url)
        );
        let done = svc.complete_flow("acp", &started.flow_id, &redirect).await;
        assert!(done.success, "{done:?}");
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
        let redirect = format!(
            "http://127.0.0.1:1/callback?{}",
            redirect_query(&auth, &started.authorization_url)
        );
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
        let redirect = format!(
            "http://127.0.0.1:1/callback?{}",
            redirect_query(&auth, &started.authorization_url)
        );
        let done = svc.complete_flow("acp", &started.flow_id, &redirect).await;
        assert!(done.success, "{done:?}");

        // Session B connects the server and has its tools.
        let (tools_b, clients_b) = connect_session(&svc, &backend, "session-b").await;
        assert!(clients_b.contains_key("linear"));
        assert!(tools_b.load().tools.contains_key("issues"));

        // Session A keeps its existing connections.
        assert!(clients_a.is_empty());
        assert!(tools_a.load().tools.is_empty());
    }

    /// Service whose agent configuration declares no MCP server; `profiles`
    /// (file name, TOML) make up the profile catalog.
    async fn profile_service(
        backend: &Arc<MemoryBackend>,
        profiles: &[(&str, String)],
    ) -> (OAuthService, tempfile::TempDir) {
        use crate::profiles::{LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager};
        let dir = tempfile::tempdir().unwrap();
        for (file, toml) in profiles {
            std::fs::write(dir.path().join(file), toml).unwrap();
        }
        let fixture = crate::test_utils::TestAgent::new().await;
        let catalog: Arc<dyn ProfileCatalog> = Arc::new(
            LocalProfileCatalog::builder()
                .include_embedded_default(false)
                .local_dir(dir.path())
                .build(),
        );
        let manager = Arc::new(ProfileRuntimeManager::with_infra_boxed(
            catalog,
            "alpha",
            crate::api::AgentInfra {
                plugin_registry: fixture.config.provider.plugin_registry(),
                storage: None,
                session_mcp_attachment_source: None,
                event_fanout: None,
            },
        ));
        let slot: crate::agent::handle::ProfileRuntimeSlot =
            Arc::new(arc_swap::ArcSwap::from_pointee(Some(manager)));
        let svc = OAuthService::new(
            fixture.config.clone(),
            ModelInventory::new(fixture.config.clone()),
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(None)),
        )
        .with_mcp_secret_backend(backend.clone())
        .with_profile_slot(slot);
        (svc, dir)
    }

    fn profile_toml(mcp: &str) -> String {
        format!("[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"p\"\n{mcp}")
    }

    fn oauth_entry(name: &str, url: &str, scopes: &str) -> String {
        format!(
            "[[mcp]]\nname = \"{name}\"\ntransport = \"http\"\nurl = \"{url}\"\nauth = {{ type = \"oauth\", scopes = [{scopes}] }}\n"
        )
    }

    #[tokio::test]
    async fn profile_only_oauth_server_is_listed_and_startable() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let url = format!("{}/mcp", auth.base);
        let (svc, _dir) = profile_service(
            &backend,
            &[(
                "alpha.toml",
                profile_toml(&oauth_entry("ibkr_read", &url, "\"mcp.read\"")),
            )],
        )
        .await;

        let statuses = svc.mcp_auth_status(None).await;
        assert_eq!(
            status_of(&statuses, "mcp:ibkr_read"),
            Some(OAuthStatus::NotAuthenticated)
        );
        let started = svc.start_flow("acp", "mcp:ibkr_read").await.unwrap();
        assert_eq!(started.provider, "mcp:ibkr_read");
        assert!(started.authorization_url.contains("scope=mcp.read"));
    }

    #[tokio::test]
    async fn identical_declarations_across_profiles_are_merged() {
        let backend = Arc::new(MemoryBackend::default());
        let entry = oauth_entry("shared", "https://mcp.example/mcp", "\"read\"");
        let (svc, _dir) = profile_service(
            &backend,
            &[
                ("alpha.toml", profile_toml(&entry)),
                ("beta.toml", profile_toml(&entry)),
            ],
        )
        .await;
        let servers = svc.oauth_servers().await;
        assert_eq!(servers.len(), 1);
        assert!(!servers[0].conflicting);
        assert_eq!(servers[0].sources, ["profile alpha", "profile beta"]);
        assert!(svc.mcp_server("shared").await.is_ok());
    }

    #[tokio::test]
    async fn conflicting_declarations_are_rejected() {
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _dir) = profile_service(
            &backend,
            &[
                (
                    "alpha.toml",
                    profile_toml(&oauth_entry(
                        "ibkr",
                        "https://api.example/mcp",
                        "\"mcp.read\"",
                    )),
                ),
                (
                    "beta.toml",
                    profile_toml(&oauth_entry(
                        "ibkr",
                        "https://api.example/mcp",
                        "\"mcp.read\", \"mcp.orders.submit\"",
                    )),
                ),
            ],
        )
        .await;
        let err = svc.start_flow("acp", "mcp:ibkr").await.unwrap_err();
        let AuthFlowError::ConflictingMcpServer { server, sources } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(server, "ibkr");
        assert_eq!(sources, &["profile alpha", "profile beta"]);
        let acp: agent_client_protocol::Error = err.into();
        assert_eq!(
            acp.code,
            agent_client_protocol::Error::invalid_params().code
        );
        let out = svc.logout("acp", "mcp:ibkr").await;
        assert!(!out.success && out.message.contains("defined differently"));
    }

    #[tokio::test]
    async fn dotagents_oauth_server_of_a_profile_is_listed() {
        let backend = Arc::new(MemoryBackend::default());
        let workspace = tempfile::tempdir().unwrap();
        let agents = workspace.path().join(".agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("mcp.json"),
            r#"{"mcpServers":{"corp":{"url":"https://corp.example/mcp",
                "auth":{"type":"oauth","scopes":["read"]}}}}"#,
        )
        .unwrap();
        let toml = format!(
            "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\nsystem = \"p\"\n\
             [dotagents]\nenabled = true\nglobal_enabled = false\nworkspace = \"{}\"\n",
            workspace.path().display()
        );
        let (svc, _dir) = profile_service(&backend, &[("alpha.toml", toml)]).await;
        let servers = svc.oauth_servers().await;
        let corp = servers
            .iter()
            .find(|d| d.server.name == "corp")
            .expect("dotagents server listed");
        assert_eq!(corp.sources, ["profile alpha (.agents)"]);
        assert_eq!(corp.server.scopes(), ["read"]);
    }

    #[tokio::test]
    async fn strict_scopes_refuse_a_wider_grant() {
        let auth = serve_auth_server().await;
        *auth.token_scope.lock() = Some("mcp.read mcp.orders.submit".into());
        let backend = Arc::new(MemoryBackend::default());
        let url = format!("{}/mcp", auth.base);
        let entry = format!(
            "[[mcp]]\nname = \"ibkr_read\"\ntransport = \"http\"\nurl = \"{url}\"\n\
             auth = {{ type = \"oauth\", scopes = [\"mcp.read\"], strict_scopes = true }}\n"
        );
        let (svc, _dir) = profile_service(&backend, &[("alpha.toml", profile_toml(&entry))]).await;

        let started = svc.start_flow("acp", "mcp:ibkr_read").await.unwrap();
        let redirect = format!(
            "http://127.0.0.1:1/callback?{}",
            redirect_query(&auth, &started.authorization_url)
        );
        let done = svc.complete_flow("acp", &started.flow_id, &redirect).await;
        assert!(!done.success, "{done:?}");
        assert!(done.message.contains("mcp.orders.submit"), "{done:?}");
        assert!(backend.0.lock().is_empty(), "nothing persisted");

        // A grant within the configured scopes is accepted.
        *auth.token_scope.lock() = Some("mcp.read".into());
        let started = svc.start_flow("acp", "mcp:ibkr_read").await.unwrap();
        let redirect = format!(
            "http://127.0.0.1:1/callback?{}",
            redirect_query(&auth, &started.authorization_url)
        );
        let done = svc.complete_flow("acp", &started.flow_id, &redirect).await;
        assert!(done.success, "{done:?}");
    }

    #[tokio::test]
    async fn refused_registration_reports_the_step_and_marks_the_span() {
        use crate::test_utils::genai_trace::{attr, capture_info};
        let auth = serve_auth_server().await;
        auth.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;

        let (result, spans) = capture_info(svc.start_flow("acp", "mcp:linear")).await;
        let err = result.unwrap_err();
        assert!(
            matches!(&err, AuthFlowError::McpAuthorization { step, .. } if *step == "client registration"),
            "{err:?}"
        );
        // The UI-facing message names the server and the step, not just
        // "Internal error".
        let acp: agent_client_protocol::Error = err.into();
        assert!(
            acp.message.contains("linear")
                && acp.message.contains("client registration")
                && acp.message.contains("403"),
            "{}",
            acp.message
        );
        assert_eq!(
            acp.code,
            agent_client_protocol::Error::internal_error().code
        );

        let start = spans
            .iter()
            .find(|s| s.name == "auth.mcp.start")
            .expect("auth.mcp.start span");
        assert!(matches!(
            start.status,
            opentelemetry::trace::Status::Error { .. }
        ));
        let error_type = attr(start, "error.type").expect("error.type").to_string();
        assert_eq!(error_type, "registration_http_403");
        assert_eq!(
            attr(start, "provider_id").map(|v| v.to_string()).as_deref(),
            Some("mcp:linear")
        );
        // Only classes and names are exported, never server text.
        assert!(!format!("{spans:?}").contains(&auth.base));
    }

    #[tokio::test]
    async fn unknown_server_is_a_user_error_with_a_readable_message() {
        use crate::test_utils::genai_trace::{attr, capture_info};
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;
        let (result, spans) = capture_info(svc.start_flow("acp", "mcp:nope")).await;
        let err = result.unwrap_err();
        assert!(err.is_user_error());
        let acp: agent_client_protocol::Error = err.into();
        assert_eq!(acp.message, "unknown OAuth MCP server: nope");
        assert_eq!(
            acp.code,
            agent_client_protocol::Error::invalid_params().code
        );
        let start = spans.iter().find(|s| s.name == "auth.mcp.start").unwrap();
        assert_eq!(
            attr(start, "error.type").map(|v| v.to_string()).as_deref(),
            Some("unknown_server")
        );
    }

    #[test]
    fn auth_target_parses_once() {
        assert_eq!(
            AuthTarget::parse(" mcp:Linear ").unwrap(),
            AuthTarget::Mcp("Linear")
        );
        assert_eq!(
            AuthTarget::parse(" Anthropic ").unwrap(),
            AuthTarget::Provider("anthropic".into())
        );
        assert!(matches!(
            AuthTarget::parse("  "),
            Err(AuthFlowError::EmptyProvider)
        ));
    }

    #[tokio::test]
    async fn logout_clears_credentials() {
        let auth = serve_auth_server().await;
        let backend = Arc::new(MemoryBackend::default());
        let (svc, _) = service(&auth, &backend, free_port()).await;
        let started = svc.start_flow("acp", "mcp:linear").await.unwrap();
        let redirect = format!(
            "http://127.0.0.1:1/callback?{}",
            redirect_query(&auth, &started.authorization_url)
        );
        let done = svc.complete_flow("acp", &started.flow_id, &redirect).await;
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
