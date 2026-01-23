//! Agent Client Protocol implementation for QueryMTAgent

use crate::agent::core::QueryMTAgent;
use crate::send_agent::SendAgent;
use agent_client_protocol::{
    AgentCapabilities, AuthenticateRequest, AuthenticateResponse, CancelNotification, Error,
    ExtNotification, ExtRequest, ExtResponse, ForkSessionRequest, ForkSessionResponse,
    Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities, McpServer,
    McpServerHttp, McpServerSse, McpServerStdio, NewSessionRequest, NewSessionResponse,
    PromptCapabilities, PromptRequest, PromptResponse, ProtocolVersion, ResumeSessionRequest,
    ResumeSessionResponse, SessionInfo, SetSessionModelRequest, SetSessionModelResponse,
};
use async_trait::async_trait;
use log::warn;
use querymt::tool_decorator::CallFunctionTool;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::{
    RoleClient,
    service::{RunningService, serve_client},
    transport::{
        SseClientTransport, StreamableHttpClientTransport, child_process::TokioChildProcess,
        sse_client::SseClientConfig, streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use std::collections::HashMap;
use std::sync::Arc;

/// SendAgent implementation for QueryMTAgent
///
/// This is the primary implementation of the agent protocol for QueryMTAgent.
/// All methods return `Send` futures, enabling true multi-threaded parallelism
/// across multiple sessions.
///
/// For compatibility with the `!Send` agent_client_protocol::Agent trait,
/// use the ApcAgentAdapter wrapper from the send_agent module.
#[async_trait]
impl SendAgent for QueryMTAgent {
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse, Error> {
        let protocol_version = if req.protocol_version <= ProtocolVersion::LATEST {
            req.protocol_version
        } else {
            ProtocolVersion::LATEST
        };

        if let Ok(mut state) = self.client_state.lock() {
            *state = Some(crate::agent::core::ClientState {
                protocol_version: protocol_version.clone(),
                client_capabilities: req.client_capabilities.clone(),
                client_info: req.client_info.clone(),
                authenticated: false,
            });
        }

        let auth_methods = self
            .auth_methods
            .lock()
            .map(|methods| methods.clone())
            .unwrap_or_default();

        let mut capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().embedded_context(true))
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true));

        // Add delegation metadata if agent registry is available
        if let Some(delegation_meta) = self.build_delegation_meta() {
            capabilities = capabilities.meta(delegation_meta);
        }

        Ok(InitializeResponse::new(protocol_version)
            .agent_capabilities(capabilities)
            .auth_methods(auth_methods)
            .agent_info(
                Implementation::new("querymt-agent", env!("CARGO_PKG_VERSION"))
                    .title("QueryMT Agent"),
            ))
    }

    async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        let auth_methods = self
            .auth_methods
            .lock()
            .map(|methods| methods.clone())
            .unwrap_or_default();

        if !auth_methods.is_empty() && !auth_methods.iter().any(|m| m.id == req.method_id) {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown auth method",
                "methodId": req.method_id.to_string(),
            })));
        }

        if let Ok(mut state) = self.client_state.lock()
            && let Some(state) = state.as_mut()
        {
            state.authenticated = true;
        }
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        if let Ok(state) = self.client_state.lock()
            && let Some(state) = state.as_ref()
        {
            let auth_required = self
                .auth_methods
                .lock()
                .map(|methods| !methods.is_empty())
                .unwrap_or(false);

            if auth_required && !state.authenticated {
                return Err(Error::auth_required());
            }
        }

        // Handle optional cwd - empty path means no cwd
        let cwd = if req.cwd.as_os_str().is_empty() {
            None
        } else {
            if !req.cwd.is_absolute() {
                return Err(Error::invalid_params().data(serde_json::json!({
                    "message": "cwd must be an absolute path",
                    "cwd": req.cwd.display().to_string(),
                })));
            }
            Some(req.cwd.clone())
        };

        let (mcp_services, mcp_tools, mcp_tool_defs) = build_mcp_state(&req.mcp_servers).await?;
        let session_context = self
            .provider
            .create_session(cwd.clone())
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;
        let session_id = session_context.session().public_id.clone();

        // Phase 3: Initialize fork if parent_session_id is provided in meta
        if let Some(_parent_id) = req
            .meta
            .as_ref()
            .and_then(|m| m.get("parent_session_id"))
            .and_then(|v| v.as_str())
        {
            // Forking is now handled via the repository/store
            // but we need to reconstruct state if this is a fresh fork point.
            // Note: In Phase 3, we expect fork point metadata to be in the session
            crate::session::runtime::SessionForkHelper::initialize_fork(
                self.provider.history_store(),
                &session_id,
            )
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;
        }

        let function_index = Arc::new(tokio::sync::OnceCell::new());

        let runtime = Arc::new(crate::agent::core::SessionRuntime {
            cwd: cwd.clone(),
            _mcp_services: mcp_services,
            mcp_tools,
            mcp_tool_defs,
            permission_cache: std::sync::Mutex::new(HashMap::new()),
            current_tools_hash: std::sync::Mutex::new(None),
            function_index,
        });

        {
            let mut runtimes = self.session_runtime.lock().await;
            runtimes.insert(session_id.clone(), runtime);
        }

        self.emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

        // Emit initial provider configuration so UI can display context limits
        if let Ok(Some(llm_config)) = self
            .provider
            .history_store()
            .get_session_llm_config(&session_id)
            .await
        {
            let context_limit =
                crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                    .and_then(|m| m.context_limit());

            self.emit_event(
                &session_id,
                crate::events::AgentEventKind::ProviderChanged {
                    provider: llm_config.provider.clone(),
                    model: llm_config.model.clone(),
                    config_id: llm_config.id,
                    context_limit,
                },
            );
        }

        if let Some(cwd_path) = cwd.clone() {
            let session_runtime = self.session_runtime.clone();
            let manager = self.workspace_index_manager.clone();
            let session_id_clone = session_id.clone();
            tokio::spawn(async move {
                let root = crate::index::resolve_workspace_root(&cwd_path);
                match manager.get_or_create(root).await {
                    Ok(workspace) => {
                        let index_handle = workspace.function_index_handle();
                        let runtime = {
                            let runtimes = session_runtime.lock().await;
                            runtimes.get(&session_id_clone).cloned()
                        };
                        if let Some(runtime) = runtime {
                            let _ = runtime.function_index.set(index_handle);
                        }
                    }
                    Err(e) => {
                        log::warn!("Failed to initialize workspace index: {}", e);
                    }
                }
            });
        }

        // Emit SessionConfigured event with environment configuration
        let mcp_configs: Vec<crate::config::McpServerConfig> = req
            .mcp_servers
            .iter()
            .map(crate::config::McpServerConfig::from_acp)
            .collect();

        self.emit_event(
            &session_id,
            crate::events::AgentEventKind::SessionConfigured {
                cwd,
                mcp_servers: mcp_configs,
            },
        );

        Ok(NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        self.run_prompt(req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        let session_id = notif.session_id.to_string();
        self.emit_event(&session_id, crate::events::AgentEventKind::Cancelled);
        let active = self.active_sessions.lock().await;
        if let Some(tx) = active.get(&session_id) {
            let _ = tx.send(true);
        }
        Ok(())
    }

    async fn load_session(&self, req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        // Validate session exists
        let session_id = req.session_id.to_string();
        let _session = self
            .provider
            .history_store()
            .get_session(&session_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "session not found",
                    "session_id": session_id,
                }))
            })?;

        // Handle optional cwd - empty path means no cwd
        let cwd = if req.cwd.as_os_str().is_empty() {
            None
        } else {
            if !req.cwd.is_absolute() {
                return Err(Error::invalid_params().data(serde_json::json!({
                    "message": "cwd must be an absolute path",
                    "cwd": req.cwd.display().to_string(),
                })));
            }
            Some(req.cwd.clone())
        };

        // Build MCP state
        let (mcp_services, mcp_tools, mcp_tool_defs) = build_mcp_state(&req.mcp_servers).await?;

        // Create SessionRuntime (no function index for load_session - would need to rebuild)
        let runtime = Arc::new(crate::agent::core::SessionRuntime {
            cwd,
            _mcp_services: mcp_services,
            mcp_tools,
            mcp_tool_defs,
            permission_cache: std::sync::Mutex::new(HashMap::new()),
            current_tools_hash: std::sync::Mutex::new(None),
            function_index: Arc::new(tokio::sync::OnceCell::new()),
        });

        {
            let mut runtimes = self.session_runtime.lock().await;
            runtimes.insert(session_id.clone(), runtime);
        }

        // Stream full history to client
        // TODO: Implement full-fidelity history streaming with SessionUpdate notifications
        // For now, we'll return success without streaming history
        self.emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

        // Emit initial provider configuration so UI can display context limits
        if let Ok(Some(llm_config)) = self
            .provider
            .history_store()
            .get_session_llm_config(&session_id)
            .await
        {
            let context_limit =
                crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                    .and_then(|m| m.context_limit());

            self.emit_event(
                &session_id,
                crate::events::AgentEventKind::ProviderChanged {
                    provider: llm_config.provider.clone(),
                    model: llm_config.model.clone(),
                    config_id: llm_config.id,
                    context_limit,
                },
            );
        }

        Ok(LoadSessionResponse::new())
    }

    async fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        let sessions = self
            .provider
            .history_store()
            .list_sessions()
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        let session_infos: Vec<SessionInfo> = sessions
            .into_iter()
            .map(|s| {
                // TODO: Store and retrieve cwd per session
                // For now, use empty path as cwd
                let mut info = SessionInfo::new(
                    agent_client_protocol::SessionId::from(s.public_id),
                    std::path::PathBuf::new(),
                );
                if let Some(name) = s.name {
                    info.title = Some(name);
                }
                if let Some(updated_at) = s.updated_at {
                    // Format as RFC3339 string
                    info.updated_at = Some(
                        updated_at
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                    );
                }
                info
            })
            .collect();

        // Apply filtering if cwd is provided
        let filtered_infos = if let Some(_cwd) = req.cwd {
            // TODO: Filter by cwd once we store cwd per session
            session_infos
        } else {
            session_infos
        };

        // Apply pagination
        let start_idx = req
            .cursor
            .as_ref()
            .and_then(|c| c.parse::<usize>().ok())
            .unwrap_or(0);
        // Default limit of 100 sessions per page
        let limit = 100;
        let end_idx = (start_idx + limit).min(filtered_infos.len());

        let paginated = filtered_infos[start_idx..end_idx].to_vec();
        let next_cursor = if end_idx < filtered_infos.len() {
            Some(end_idx.to_string())
        } else {
            None
        };

        Ok(ListSessionsResponse::new(paginated).next_cursor(next_cursor))
    }

    async fn fork_session(&self, req: ForkSessionRequest) -> Result<ForkSessionResponse, Error> {
        let source_session_id = req.session_id.to_string();

        // Validate source session exists
        let _session = self
            .provider
            .history_store()
            .get_session(&source_session_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "source session not found",
                    "session_id": source_session_id,
                }))
            })?;

        // Get last message ID from history
        let history = self
            .provider
            .history_store()
            .get_history(&source_session_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        let target_message_id = history
            .last()
            .map(|msg| msg.id.clone())
            .ok_or_else(|| Error::new(-32000, "cannot fork empty session"))?;

        // Fork the session
        let new_session_id = self
            .provider
            .history_store()
            .fork_session(
                &source_session_id,
                &target_message_id,
                crate::session::domain::ForkOrigin::User,
            )
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        Ok(ForkSessionResponse::new(new_session_id))
    }

    async fn resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        // Same as load_session but skip history streaming
        let session_id = req.session_id.to_string();
        let _session = self
            .provider
            .history_store()
            .get_session(&session_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "session not found",
                    "session_id": session_id,
                }))
            })?;

        // Handle optional cwd - empty path means no cwd
        let cwd = if req.cwd.as_os_str().is_empty() {
            None
        } else {
            if !req.cwd.is_absolute() {
                return Err(Error::invalid_params().data(serde_json::json!({
                    "message": "cwd must be an absolute path",
                    "cwd": req.cwd.display().to_string(),
                })));
            }
            Some(req.cwd.clone())
        };

        // Build MCP state
        let (mcp_services, mcp_tools, mcp_tool_defs) = build_mcp_state(&req.mcp_servers).await?;

        // Create SessionRuntime (no function index for resume_session)
        let runtime = Arc::new(crate::agent::core::SessionRuntime {
            cwd,
            _mcp_services: mcp_services,
            mcp_tools,
            mcp_tool_defs,
            permission_cache: std::sync::Mutex::new(HashMap::new()),
            current_tools_hash: std::sync::Mutex::new(None),
            function_index: Arc::new(tokio::sync::OnceCell::new()),
        });

        {
            let mut runtimes = self.session_runtime.lock().await;
            runtimes.insert(session_id.clone(), runtime);
        }

        self.emit_event(&session_id, crate::events::AgentEventKind::SessionCreated);

        Ok(ResumeSessionResponse::new())
    }

    async fn set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        let session_id = req.session_id.to_string();

        // Validate session exists
        let _session = self
            .provider
            .history_store()
            .get_session(&session_id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "session not found",
                    "session_id": session_id,
                }))
            })?;

        // Parse model_id format: "provider/model" or just "model"
        let model_id = req.model_id.to_string();
        let (provider, model) = if let Some(slash_pos) = model_id.find('/') {
            let provider = &model_id[..slash_pos];
            let model = &model_id[slash_pos + 1..];
            (provider.to_string(), model.to_string())
        } else {
            // If no provider specified, use the current provider
            let current_config = self
                .provider
                .history_store()
                .get_session_llm_config(&session_id)
                .await
                .map_err(|e| Error::new(-32000, e.to_string()))?;

            let provider = current_config
                .map(|c| c.provider)
                .unwrap_or_else(|| "anthropic".to_string());
            (provider, model_id)
        };

        // Create or get LLM config
        let llm_config_input = querymt::LLMParams::new()
            .provider(provider.clone())
            .model(model.clone());
        let llm_config = self
            .provider
            .history_store()
            .create_or_get_llm_config(&llm_config_input)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        // Update session's LLM config
        self.provider
            .history_store()
            .set_session_llm_config(&session_id, llm_config.id)
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        // Note: We don't update the global provider here as that would affect all sessions.
        // The session-specific LLM config is stored in the database and will be used
        // when creating a SessionContext for this session.

        Ok(SetSessionModelResponse::new())
    }

    async fn ext_method(&self, _req: ExtRequest) -> Result<ExtResponse, Error> {
        // Return empty response - extensions not yet implemented
        let raw_value = serde_json::value::RawValue::from_string("null".to_string())
            .map_err(|e| Error::new(-32000, e.to_string()))?;
        Ok(ExtResponse::new(Arc::from(raw_value)))
    }

    async fn ext_notification(&self, _notif: ExtNotification) -> Result<(), Error> {
        // OK - extensions not yet implemented
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Builds MCP state by connecting to configured servers and returns:
/// - RunningService instances (must be kept alive to maintain connections)
/// - McpToolAdapter instances (for tool execution)
/// - Tool definitions (for LLM)
async fn build_mcp_state(
    servers: &[McpServer],
) -> Result<
    (
        HashMap<String, RunningService<RoleClient, ()>>,
        HashMap<String, Arc<querymt::mcp::adapter::McpToolAdapter>>,
        Vec<querymt::chat::Tool>,
    ),
    Error,
> {
    let mut clients = HashMap::new();
    let mut tools = HashMap::new();
    let mut tool_defs = Vec::new();

    for server in servers {
        let (server_name, running): (String, RunningService<RoleClient, ()>) =
            start_mcp_server(server).await?;
        let peer = running.peer().clone();
        let tool_list = peer
            .list_all_tools()
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;

        for tool in tool_list {
            let adapter = querymt::mcp::adapter::McpToolAdapter::try_new(
                tool,
                peer.clone(),
                server_name.clone(),
            )
            .map_err(|e| Error::new(-32000, e.to_string()))?;
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

    Ok((clients, tools, tool_defs))
}

/// Starts an MCP server based on its configuration
async fn start_mcp_server(
    server: &McpServer,
) -> Result<(String, RunningService<RoleClient, ()>), Error> {
    match server {
        McpServer::Stdio(stdio) => {
            let McpServerStdio {
                name,
                command,
                args,
                env,
                ..
            } = stdio.clone();
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .envs(env.iter().map(|item| (&item.name, &item.value)))
                .stderr(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::piped())
                .stdin(std::process::Stdio::piped());
            let transport =
                TokioChildProcess::new(cmd).map_err(|e| Error::new(-32000, e.to_string()))?;
            let running: RunningService<RoleClient, ()> =
                serve_client((), transport).await.map_err(|e| {
                    Error::new(-32000, format!("failed to start MCP stdio server: {}", e))
                })?;
            Ok((name, running))
        }
        McpServer::Http(http) => {
            let McpServerHttp {
                name, url, headers, ..
            } = http.clone();
            let client = reqwest::ClientBuilder::new()
                .default_headers(headers_to_map(&headers)?)
                .build()
                .map_err(|e| Error::new(-32000, e.to_string()))?;
            let transport = StreamableHttpClientTransport::with_client(
                client,
                StreamableHttpClientTransportConfig::with_uri(url),
            );
            let running = serve_client((), transport).await.map_err(|e| {
                Error::new(-32000, format!("failed to start MCP http server: {}", e))
            })?;
            Ok((name, running))
        }
        McpServer::Sse(sse) => {
            let McpServerSse {
                name, url, headers, ..
            } = sse.clone();
            let client = reqwest::ClientBuilder::new()
                .default_headers(headers_to_map(&headers)?)
                .build()
                .map_err(|e| Error::new(-32000, e.to_string()))?;
            let transport = SseClientTransport::start_with_client(
                client,
                SseClientConfig {
                    sse_endpoint: url.into(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| Error::new(-32000, e.to_string()))?;
            let running = serve_client((), transport).await.map_err(|e| {
                Error::new(-32000, format!("failed to start MCP sse server: {}", e))
            })?;
            Ok((name, running))
        }
        _ => Err(Error::invalid_params().data(serde_json::json!({
            "message": "unsupported MCP server configuration",
        }))),
    }
}

/// Converts HTTP headers from protocol format to reqwest format
fn headers_to_map(headers: &[agent_client_protocol::HttpHeader]) -> Result<HeaderMap, Error> {
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
