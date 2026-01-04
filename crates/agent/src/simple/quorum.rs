//! Multi-agent quorum implementation

use super::callbacks::EventCallbacksState;
use super::config::{AgentConfig, DelegateConfigBuilder, PlannerConfigBuilder};
use super::utils::{
    build_llm_config, default_registry, infer_required_capabilities, latest_assistant_message,
    to_absolute_path,
};
use crate::agent::builder::AgentBuilderExt;
use crate::agent::core::{QueryMTAgent, ToolPolicy};
use crate::config::{QuorumConfig, resolve_tools};
use crate::delegation::AgentInfo;
use crate::events::AgentEvent;
use crate::quorum::AgentQuorum;
use crate::runner::{ChatRunner, ChatSession};
use crate::send_agent::SendAgent;
use crate::server::AgentServer;
use crate::session::projection::ViewStore;
use crate::tools::CapabilityRequirement;
use crate::tools::builtins::all_builtin_tools;
use agent_client_protocol::{ContentBlock, NewSessionRequest, PromptRequest, TextContent};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Builder for multi-agent quorum with closure-based configuration
pub struct QuorumBuilder {
    pub(super) cwd: Option<PathBuf>,
    pub(super) db_path: Option<PathBuf>,
    pub(super) planner_config: Option<AgentConfig>,
    pub(super) delegates: Vec<AgentConfig>,
    pub(super) delegation_enabled: bool,
    pub(super) verification_enabled: bool,
}

impl Default for QuorumBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl QuorumBuilder {
    pub fn new() -> Self {
        Self {
            cwd: None,
            db_path: None,
            planner_config: None,
            delegates: Vec::new(),
            delegation_enabled: true,
            verification_enabled: false,
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn db(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    /// Configure the planner agent using a closure
    pub fn planner<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(PlannerConfigBuilder) -> PlannerConfigBuilder,
    {
        let builder = PlannerConfigBuilder::new();
        self.planner_config = Some(configure(builder).build());
        self
    }

    /// Add a delegate agent configured via closure
    pub fn delegate<F>(mut self, id: impl Into<String>, configure: F) -> Self
    where
        F: FnOnce(DelegateConfigBuilder) -> DelegateConfigBuilder,
    {
        let builder = DelegateConfigBuilder::new(id);
        self.delegates.push(configure(builder).build());
        self
    }

    pub fn with_delegation(mut self, enabled: bool) -> Self {
        self.delegation_enabled = enabled;
        self
    }

    pub fn with_verification(mut self, enabled: bool) -> Self {
        self.verification_enabled = enabled;
        self
    }

    pub fn with_defaults(mut self) -> Self {
        self.delegation_enabled = true;
        self.verification_enabled = true;
        self
    }

    pub async fn build(self) -> Result<Quorum> {
        let planner_config = self
            .planner_config
            .ok_or_else(|| anyhow!("Planner configuration is required"))?;

        // Convert cwd to absolute path if provided
        let cwd = self.cwd.map(to_absolute_path).transpose()?;

        // Capability validation
        let mut all_required = HashSet::new();
        all_required.extend(infer_required_capabilities(&planner_config.tools));
        for delegate in &self.delegates {
            all_required.extend(&delegate.required_capabilities);
        }

        if all_required.contains(&CapabilityRequirement::Filesystem) && cwd.is_none() {
            return Err(anyhow!(
                "Working directory required: one or more agents require filesystem access. Use .cwd() to set one."
            ));
        }

        let registry = Arc::new(default_registry().await?);

        let mut builder = AgentQuorum::builder(self.db_path)
            .await
            .map_err(|e| anyhow!(format!("Failed to build quorum: {e}")))?;

        if let Some(cwd_path) = cwd.clone() {
            builder = builder.cwd(cwd_path);
        }

        for delegate in self.delegates {
            let agent_info = AgentInfo {
                id: delegate.id.clone(),
                name: delegate.id.clone(),
                description: delegate.description.clone().unwrap_or_default(),
                capabilities: delegate.capabilities.clone(),
                required_capabilities: delegate.required_capabilities.clone(),
                meta: None,
            };
            let llm_config = build_llm_config(&delegate)?;
            let tools = delegate.tools.clone();
            let registry = registry.clone();
            builder = builder.add_delegate_agent(agent_info, move |store, event_bus| {
                let mut agent = QueryMTAgent::new(registry.clone(), store, llm_config.clone())
                    .with_event_bus(event_bus)
                    .with_tool_policy(ToolPolicy::BuiltInOnly);
                if !tools.is_empty() {
                    agent = agent.with_allowed_tools(tools.clone());
                }
                Arc::new(agent)
            });
        }

        let planner_llm = build_llm_config(&planner_config)?;
        let planner_tools = planner_config.tools.clone();
        let registry_for_planner = registry.clone();
        builder = builder.with_planner(move |store, event_bus, agent_registry| {
            let mut agent =
                QueryMTAgent::new(registry_for_planner.clone(), store, planner_llm.clone())
                    .with_event_bus(event_bus)
                    .with_agent_registry(agent_registry);
            if !planner_tools.is_empty() {
                agent = agent
                    .with_tool_policy(ToolPolicy::BuiltInOnly)
                    .with_allowed_tools(planner_tools.clone());
            }
            Arc::new(agent)
        });

        builder = builder
            .with_delegation(self.delegation_enabled)
            .with_verification(self.verification_enabled);

        let quorum = builder.build().map_err(|e| anyhow!(e.to_string()))?;
        let view_store = quorum.view_store();
        Ok(Quorum {
            inner: quorum,
            view_store,
            planner_session_id: Arc::new(Mutex::new(None)),
            cwd,
            callbacks: Arc::new(EventCallbacksState::new(None)),
        })
    }
}

pub struct Quorum {
    inner: AgentQuorum,
    view_store: Arc<dyn ViewStore>,
    planner_session_id: Arc<Mutex<Option<String>>>,
    cwd: Option<PathBuf>,
    callbacks: Arc<EventCallbacksState>,
}

impl Quorum {
    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let session_id = self.ensure_planner_session().await?;
        let request = PromptRequest::new(
            session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        let planner = self.inner.planner();
        planner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = planner
            .provider
            .history_store()
            .get_history(&session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    pub fn inner(&self) -> &AgentQuorum {
        &self.inner
    }

    pub fn planner(&self) -> Arc<QueryMTAgent> {
        self.inner.planner()
    }

    pub fn delegate(&self, id: &str) -> Option<Arc<QueryMTAgent>> {
        self.inner.delegate(id)
    }

    pub fn dashboard(&self) -> AgentServer {
        AgentServer::new(self.planner(), self.view_store.clone())
    }

    /// Start an ACP server with the specified transport.
    ///
    /// # Arguments
    /// * `transport` - Either "stdio" for stdin/stdout, or "ip:port" for WebSocket
    ///
    /// # Example
    /// ```rust,no_run
    /// # use querymt_agent::prelude::*;
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// let quorum = Agent::multi()
    ///     .planner(|p| p.provider("openai", "gpt-4"))
    ///     .build()
    ///     .await?;
    ///     
    /// quorum.acp("stdio").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acp(&self, transport: &str) -> Result<()> {
        match transport {
            "stdio" => crate::acp::serve_stdio(self.planner())
                .await
                .map_err(|e| anyhow!("ACP stdio error: {}", e)),
            addr if addr.contains(':') => Err(anyhow!(
                "WebSocket ACP not yet implemented for Quorum. Use .dashboard().run(\"{}\") for web access.",
                addr
            )),
            _ => Err(anyhow!(
                "Invalid ACP transport '{}'. Use 'stdio' or 'ip:port' format.",
                transport
            )),
        }
    }

    async fn ensure_planner_session(&self) -> Result<String> {
        if let Some(existing) = self.planner_session_id.lock().unwrap().clone() {
            return Ok(existing);
        }
        let planner = self.inner.planner();
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = planner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let session_id = response.session_id.to_string();
        *self.planner_session_id.lock().unwrap() = Some(session_id.clone());
        Ok(session_id)
    }

    async fn create_new_planner_session(&self) -> Result<String> {
        let planner = self.inner.planner();
        let request = match &self.cwd {
            Some(cwd) => NewSessionRequest::new(cwd.clone()),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = planner
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(response.session_id.to_string())
    }

    /// Build a Quorum from a quorum config
    pub async fn from_quorum_config(config: QuorumConfig) -> Result<Self> {
        let mut builder = QuorumBuilder::new();

        if let Some(cwd) = config.quorum.cwd {
            builder = builder.cwd(cwd);
        }
        if let Some(db) = config.quorum.db {
            builder = builder.db(db);
        }

        builder.delegation_enabled = config.quorum.delegation;
        builder.verification_enabled = config.quorum.verification;

        // Build the set of builtin tool names for validation
        let builtin_names: HashSet<String> = all_builtin_tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();

        // Configure planner with tool resolution
        let mut planner_config = AgentConfig::new("planner");
        let mut llm = querymt::LLMParams::new()
            .provider(config.planner.provider)
            .model(config.planner.model);
        if let Some(system) = config.planner.system {
            llm = llm.system(system);
        }
        if let Some(api_key) = config.planner.api_key {
            llm = llm.api_key(api_key);
        }
        if let Some(params) = config.planner.parameters {
            for (key, value) in params {
                llm = llm.parameter(key, value);
            }
        }
        planner_config.llm_config = Some(llm);

        // Resolve planner tools (validates builtin tools and prepares for MCP)
        let planner_resolved =
            resolve_tools(&config.planner.tools, &config.mcp, &[], &builtin_names)?;
        planner_config.tools = planner_resolved.builtins;

        // Note: MCP tools are not yet supported in the simple Quorum API.
        // MCP servers would need to be started when sessions are created,
        // similar to how it's done in protocol.rs. For now, we only validate
        // and extract builtin tools.
        if !planner_resolved.mcp_servers.is_empty() {
            log::warn!(
                "MCP servers configured for planner, but MCP is not yet supported in Quorum. Only builtin tools will be available."
            );
        }

        builder.planner_config = Some(planner_config);

        // Configure delegates with tool resolution
        for delegate in config.delegates {
            let mut delegate_config = AgentConfig::new(delegate.id.clone());
            let mut llm = querymt::LLMParams::new()
                .provider(delegate.provider)
                .model(delegate.model);
            if let Some(system) = delegate.system {
                llm = llm.system(system);
            }
            if let Some(api_key) = delegate.api_key {
                llm = llm.api_key(api_key);
            }
            if let Some(params) = delegate.parameters {
                for (key, value) in params {
                    llm = llm.parameter(key, value);
                }
            }
            delegate_config.llm_config = Some(llm);
            delegate_config.description = delegate.description;
            delegate_config.capabilities = delegate.capabilities;

            // Resolve delegate tools
            let delegate_resolved =
                resolve_tools(&delegate.tools, &config.mcp, &delegate.mcp, &builtin_names)?;
            delegate_config.tools = delegate_resolved.builtins;

            if !delegate_resolved.mcp_servers.is_empty() {
                log::warn!(
                    "MCP servers configured for delegate '{}', but MCP is not yet supported in Quorum. Only builtin tools will be available.",
                    delegate.id
                );
            }

            delegate_config.required_capabilities =
                infer_required_capabilities(&delegate_config.tools)
                    .into_iter()
                    .collect();
            builder.delegates.push(delegate_config);
        }

        builder.build().await
    }
}

#[async_trait]
impl ChatRunner for Quorum {
    async fn chat(&self, prompt: &str) -> Result<String> {
        Quorum::chat(self, prompt).await
    }

    async fn chat_session(&self) -> Result<Box<dyn ChatSession>> {
        let session_id = self.create_new_planner_session().await?;
        let session = QuorumSession::new(self.inner.planner(), session_id, self.cwd.clone());
        Ok(Box::new(session))
    }

    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.inner.planner().subscribe_events()
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.inner.planner().subscribe_events());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.inner.planner().subscribe_events());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.inner.planner().subscribe_events());
    }

    fn on_delegation_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_delegation(callback);
        self.callbacks
            .ensure_listener(self.inner.planner().subscribe_events());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.inner.planner().subscribe_events());
    }

    fn dashboard(&self) -> AgentServer {
        Quorum::dashboard(self)
    }
}

/// A session for interacting with a Quorum's planner agent
pub struct QuorumSession {
    planner: Arc<QueryMTAgent>,
    session_id: String,
    callbacks: Arc<EventCallbacksState>,
    #[allow(dead_code)]
    cwd: Option<PathBuf>,
}

impl QuorumSession {
    fn new(planner: Arc<QueryMTAgent>, session_id: String, cwd: Option<PathBuf>) -> Self {
        let callbacks = Arc::new(EventCallbacksState::new(Some(session_id.clone())));
        Self {
            planner,
            session_id,
            callbacks,
            cwd,
        }
    }
}

#[async_trait]
impl ChatSession for QuorumSession {
    fn id(&self) -> &str {
        &self.session_id
    }

    async fn chat(&self, prompt: &str) -> Result<String> {
        let request = PromptRequest::new(
            self.session_id.clone(),
            vec![ContentBlock::Text(TextContent::new(prompt))],
        );
        self.planner
            .prompt(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let history = self
            .planner
            .provider
            .history_store()
            .get_history(&self.session_id)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        latest_assistant_message(&history).ok_or_else(|| anyhow!("No assistant response found"))
    }

    fn on_tool_call_boxed(&self, callback: Box<dyn Fn(String, Value) + Send + Sync>) {
        self.callbacks.on_tool_call(callback);
        self.callbacks
            .ensure_listener(self.planner.subscribe_events());
    }

    fn on_tool_complete_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_tool_complete(callback);
        self.callbacks
            .ensure_listener(self.planner.subscribe_events());
    }

    fn on_message_boxed(&self, callback: Box<dyn Fn(String, String) + Send + Sync>) {
        self.callbacks.on_message(callback);
        self.callbacks
            .ensure_listener(self.planner.subscribe_events());
    }

    fn on_error_boxed(&self, callback: Box<dyn Fn(String) + Send + Sync>) {
        self.callbacks.on_error(callback);
        self.callbacks
            .ensure_listener(self.planner.subscribe_events());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quorum_delegate_builder_system() {
        let builder = QuorumBuilder::new()
            .cwd(PathBuf::from("/tmp"))
            .planner(|p| {
                p.provider("openai", "gpt-4")
                    .system("Planner system prompt")
                    .tools(["delegate"])
            })
            .delegate("coder", |d| {
                d.provider("ollama", "model")
                    .system("Coder system prompt")
                    .tools(["shell"])
            });

        // Test that the delegate was added with correct system prompt
        assert_eq!(builder.delegates.len(), 1);
        let delegate = &builder.delegates[0];
        assert_eq!(
            delegate.llm_config.as_ref().and_then(|c| c.system.clone()),
            Some("Coder system prompt".to_string())
        );
    }
}
