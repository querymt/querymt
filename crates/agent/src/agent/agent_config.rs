//! Shared agent configuration and infrastructure.
//!
//! `AgentConfig` is constructed once at build time and wrapped in `Arc`.
//! It holds all shared/immutable state that session actors need. It is NOT
//! an actor — it has no lifecycle or message processing needs.

use crate::agent::core::{
    AgentMode, DelegationContextConfig, SnapshotPolicy, ToolConfig, ToolPolicy,
};
use crate::config::RuntimeExecutionPolicy;
use crate::delegation::AgentRegistry;
use crate::event_bus::EventBus;
use crate::index::WorkspaceIndexManagerActor;
use crate::middleware::{CompositeDriver, MiddlewareDriver};
use crate::session::compaction::SessionCompaction;
use crate::session::provider::SessionProvider;
use crate::session::store::SessionExecutionConfig;
use crate::tools::ToolRegistry;
use agent_client_protocol::AuthMethod;
use kameo::actor::ActorRef;
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

/// Shared agent configuration and infrastructure.
///
/// Constructed once at build time. Wrapped in `Arc` and passed to each
/// `SessionActor` at spawn. Not an actor — no lifecycle, no message processing.
///
/// For rare agent-wide config changes (e.g., updating tool policy across all sessions),
/// the server layer broadcasts messages to all active session actors.
pub struct AgentConfig {
    // ── Infrastructure (Arc, thread-safe) ────────────────────────
    pub provider: Arc<SessionProvider>,
    pub event_bus: Arc<EventBus>,
    pub agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    pub workspace_manager_actor: ActorRef<WorkspaceIndexManagerActor>,

    // ── Defaults (used when spawning new sessions) ───────────────
    /// Shared live reference to the default agent mode.
    /// Session actors read this at spawn time to get the current mode.
    pub default_mode: Arc<StdMutex<AgentMode>>,
    pub tool_config: ToolConfig,
    pub tool_registry: ToolRegistry,
    pub middleware_drivers: Vec<Arc<dyn MiddlewareDriver>>,
    pub auth_methods: Vec<AuthMethod>,

    // ── Execution config ─────────────────────────────────────────
    pub max_steps: Option<usize>,
    pub snapshot_policy: SnapshotPolicy,
    pub assume_mutating: bool,
    pub mutating_tools: HashSet<String>,
    pub max_prompt_bytes: Option<usize>,
    pub execution_timeout_secs: u64,
    /// Grouped execution policy: tool output, pruning, compaction, rate limit.
    pub execution_policy: RuntimeExecutionPolicy,
    pub compaction: SessionCompaction,
    pub snapshot_backend: Option<Arc<dyn crate::snapshot::SnapshotBackend>>,
    pub snapshot_gc_config: crate::snapshot::GcConfig,
    pub delegation_context_config: DelegationContextConfig,
    pub pending_elicitations: crate::elicitation::PendingElicitationMap,
}

impl AgentConfig {
    /// Creates a `CompositeDriver` from the configured middleware drivers.
    pub fn create_driver(&self) -> CompositeDriver {
        use crate::middleware::{LimitsConfig, LimitsMiddleware};

        let mut drivers: Vec<Arc<dyn MiddlewareDriver>> = Vec::new();

        // Add LimitsMiddleware if configured
        if let Some(max_steps) = self.max_steps {
            drivers.push(Arc::new(LimitsMiddleware::new(
                LimitsConfig::default().max_steps(max_steps),
            )));
        }

        // Add all user-configured middleware drivers
        for driver in &self.middleware_drivers {
            drivers.push(driver.clone());
        }

        CompositeDriver::new(drivers)
    }

    /// Returns the session limits from configured middleware.
    pub fn get_session_limits(&self) -> Option<crate::events::SessionLimits> {
        self.create_driver().get_limits()
    }

    /// Builds delegation metadata for ACP `AgentCapabilities._meta` field.
    pub fn build_delegation_meta(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        let agents = self.agent_registry.list_agents();
        if agents.is_empty() {
            return None;
        }

        let delegation_value = serde_json::json!({
            "version": "1",
            "available": true,
            "agents": agents.iter().map(|agent| {
                serde_json::json!({
                    "id": agent.id,
                    "name": agent.name,
                    "description": agent.description,
                    "capabilities": agent.capabilities,
                })
            }).collect::<Vec<_>>()
        });

        let mut meta = serde_json::Map::new();
        meta.insert("mt.query.agent.delegation".to_string(), delegation_value);
        Some(meta)
    }

    /// Checks if a tool requires permission for execution.
    pub fn requires_permission_for_tool(&self, tool_name: &str) -> bool {
        self.mutating_tools.contains(tool_name)
            || matches!(
                crate::agent::utils::tool_kind_for_tool(tool_name),
                agent_client_protocol::ToolKind::Edit
                    | agent_client_protocol::ToolKind::Delete
                    | agent_client_protocol::ToolKind::Execute
            )
    }

    /// Emits an event for external observers.
    pub fn emit_event(&self, session_id: &str, kind: crate::events::AgentEventKind) {
        self.event_bus.publish(session_id, kind);
    }

    /// Subscribes to agent events.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<crate::events::AgentEvent> {
        self.event_bus.subscribe()
    }

    /// Access the underlying event bus.
    pub fn event_bus(&self) -> Arc<EventBus> {
        self.event_bus.clone()
    }

    /// Access the agent registry.
    pub fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.agent_registry.clone()
    }

    /// Access the tool registry (clone).
    pub fn tool_registry_arc(&self) -> Arc<ToolRegistry> {
        Arc::new(self.tool_registry.clone())
    }

    /// Access the workspace manager actor ref.
    pub fn workspace_manager_actor(&self) -> ActorRef<WorkspaceIndexManagerActor> {
        self.workspace_manager_actor.clone()
    }

    /// Access the pending elicitations map.
    pub fn pending_elicitations(&self) -> crate::elicitation::PendingElicitationMap {
        self.pending_elicitations.clone()
    }

    /// Determines if a tool should trigger snapshotting.
    #[allow(dead_code)]
    pub fn should_snapshot_tool(&self, tool_name: &str) -> bool {
        self.mutating_tools.contains(tool_name) || self.assume_mutating
    }

    /// Prepares a snapshot configuration if enabled.
    #[allow(dead_code)]
    pub fn prepare_snapshot(
        &self,
        cwd: Option<&std::path::Path>,
    ) -> Option<(std::path::PathBuf, SnapshotPolicy)> {
        if self.snapshot_policy == SnapshotPolicy::None {
            return None;
        }
        let root = cwd?.to_path_buf();
        Some((root, self.snapshot_policy))
    }

    /// Checks if a tool is allowed by current configuration.
    pub fn is_tool_allowed(&self, name: &str) -> bool {
        crate::agent::tools::is_tool_allowed_with(&self.tool_config, name)
    }

    /// Gets a snapshot of the current tool configuration.
    pub fn tool_config_snapshot(&self) -> ToolConfig {
        self.tool_config.clone()
    }

    /// Access the session provider.
    pub fn provider(&self) -> &Arc<SessionProvider> {
        &self.provider
    }

    /// Add an event observer.
    pub fn add_observer(&self, observer: Arc<dyn crate::events::EventObserver>) {
        self.event_bus.add_observer(observer);
    }

    /// Add multiple event observers.
    pub fn add_observers(&self, observers: Vec<Arc<dyn crate::events::EventObserver>>) {
        self.event_bus.add_observers(observers);
    }

    /// Gracefully shutdown the event bus.
    pub async fn shutdown(&self) {
        self.event_bus.shutdown().await;
    }

    /// Get LLM config by ID.
    pub async fn get_llm_config(
        &self,
        config_id: i64,
    ) -> Result<Option<crate::session::store::LLMConfig>, agent_client_protocol::Error> {
        self.provider
            .history_store()
            .get_llm_config(config_id)
            .await
            .map_err(|e| agent_client_protocol::Error::new(-32000, e.to_string()))
    }

    /// Get a session by ID from the store.
    pub async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::session::store::Session>, agent_client_protocol::Error> {
        self.provider
            .history_store()
            .get_session(session_id)
            .await
            .map_err(|e| agent_client_protocol::Error::new(-32000, e.to_string()))
    }

    /// Collects available tools based on current configuration.
    pub fn collect_tools(
        &self,
        provider: Arc<dyn querymt::LLMProvider>,
        runtime: Option<&crate::agent::core::SessionRuntime>,
    ) -> Vec<querymt::chat::Tool> {
        let mut tools = Vec::new();
        let config = &self.tool_config;

        match config.policy {
            ToolPolicy::BuiltInOnly => {
                tools.extend(self.tool_registry.definitions());
            }
            ToolPolicy::ProviderOnly => {
                if let Some(provider_tools) = provider.tools() {
                    tools.extend(provider_tools.iter().cloned::<querymt::chat::Tool>());
                }
            }
            ToolPolicy::BuiltInAndProvider => {
                tools.extend(self.tool_registry.definitions());
                if let Some(provider_tools) = provider.tools() {
                    tools.extend(provider_tools.iter().cloned::<querymt::chat::Tool>());
                }
            }
        }

        if let (Some(runtime), ToolPolicy::ProviderOnly | ToolPolicy::BuiltInAndProvider) =
            (runtime, config.policy)
        {
            tools.extend(runtime.mcp_tool_defs.iter().cloned());
        }

        tools
            .into_iter()
            .filter(|tool| crate::agent::tools::is_tool_allowed_with(config, &tool.function.name))
            .collect()
    }

    pub fn execution_config_snapshot(&self) -> SessionExecutionConfig {
        SessionExecutionConfig {
            max_steps: self.max_steps,
            max_prompt_bytes: self.max_prompt_bytes,
            execution_timeout_secs: self.execution_timeout_secs,
            snapshot_policy: self.snapshot_policy.to_string(),
            tool_output_config: self.execution_policy.tool_output.clone(),
            pruning_config: self.execution_policy.pruning.clone(),
            compaction_config: self.execution_policy.compaction.clone(),
            rate_limit_config: self.execution_policy.rate_limit.clone(),
        }
    }

    pub async fn invalidate_provider_cache(&self) {
        self.provider.clear_provider_cache().await;
    }
}
