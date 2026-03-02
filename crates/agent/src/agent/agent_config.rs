//! Shared agent configuration and infrastructure.
//!
//! `AgentConfig` is constructed once at build time and wrapped in `Arc`.
//! It holds all shared/immutable state that session actors need. It is NOT
//! an actor — it has no lifecycle or message processing needs.

use crate::agent::core::{
    AgentMode, DelegationContextConfig, SnapshotPolicy, ToolConfig, ToolPolicy,
};
use crate::config::{DelegationWaitPolicy, McpServerConfig, RuntimeExecutionPolicy};
use crate::delegation::AgentRegistry;
use crate::event_sink::EventSink;
use crate::events::{AgentEventKind, DurableEvent};
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
#[derive(Clone)]
pub struct AgentConfig {
    // ── Infrastructure (Arc, thread-safe) ────────────────────────
    pub provider: Arc<SessionProvider>,
    pub event_sink: Arc<EventSink>,
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

    // ── MCP servers (from TOML config, applied to every session) ────────
    /// MCP servers defined in the TOML `[[mcp]]` section.
    /// These are merged into every `NewSessionRequest` automatically.
    pub mcp_servers: Vec<McpServerConfig>,

    // ── Execution config ─────────────────────────────────────────
    pub max_steps: Option<usize>,
    pub snapshot_policy: SnapshotPolicy,
    pub assume_mutating: bool,
    pub mutating_tools: HashSet<String>,
    pub max_prompt_bytes: Option<usize>,
    pub execution_timeout_secs: u64,
    pub delegation_wait_policy: DelegationWaitPolicy,
    pub delegation_wait_timeout_secs: u64,
    pub delegation_cancel_grace_secs: u64,
    /// Grouped execution policy: tool output, pruning, compaction, rate limit.
    pub execution_policy: RuntimeExecutionPolicy,
    pub compaction: SessionCompaction,
    pub snapshot_backend: Option<Arc<dyn crate::snapshot::SnapshotBackend>>,
    pub snapshot_gc_config: crate::snapshot::GcConfig,
    pub delegation_context_config: DelegationContextConfig,
    pub pending_elicitations: crate::elicitation::PendingElicitationMap,
}

impl AgentConfig {
    /// Create a copy with `middleware_drivers` replaced.
    ///
    /// Used by `AgentBuilder::build()` to swap in the final middleware chain
    /// without manually cloning every field.
    pub(crate) fn with_middleware(mut self, drivers: Vec<Arc<dyn MiddlewareDriver>>) -> Self {
        self.middleware_drivers = drivers;
        self
    }

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

    /// Emits an event through the EventSink (auto-classifies as durable/ephemeral).
    ///
    /// For durable events this spawns an async task that persists and publishes.
    /// For ephemeral events this publishes immediately (no persistence).
    pub fn emit_event(&self, session_id: &str, kind: AgentEventKind) {
        use crate::events::{Durability, classify_durability};

        // Ephemeral events: publish immediately via sink, no persistence.
        if classify_durability(&kind) == Durability::Ephemeral {
            self.event_sink.emit_ephemeral(session_id, kind);
            return;
        }

        // Durable events: persist via EventSink journal in a spawned task.
        let sink = self.event_sink.clone();
        let session_id = session_id.to_string();

        tokio::spawn(async move {
            if let Err(err) = sink.emit_durable(&session_id, kind).await {
                log::warn!(
                    "failed to emit durable event for session {}: {}",
                    session_id,
                    err
                );
            }
        });
    }

    /// Persists and publishes a durable event, returning the `DurableEvent`.
    ///
    /// Use this path when caller-side ordering matters (awaited).
    pub async fn emit_event_persisted(
        &self,
        session_id: &str,
        kind: AgentEventKind,
    ) -> crate::session::error::SessionResult<DurableEvent> {
        self.event_sink.emit_durable(session_id, kind).await
    }

    /// Subscribes to agent events via the fanout (live stream).
    pub fn subscribe_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::events::EventEnvelope> {
        self.event_sink.fanout().subscribe()
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

    /// Gracefully shutdown.
    pub async fn shutdown(&self) {
        // No-op: EventBus observer infrastructure has been removed.
        // EventFanout broadcast subscribers are dropped automatically.
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
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))
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
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))
    }

    /// Collects available tools based on current configuration.
    ///
    /// `tool_config_override` is the per-session tool configuration captured at the
    /// start of a turn (from `SessionActor.tool_config`). When `Some`, it takes
    /// precedence over the global `AgentConfig.tool_config`, allowing runtime
    /// mutations via `SetAllowedTools` / `SetDeniedTools` / `SetToolPolicy` to
    /// take effect. When `None`, falls back to the global config.
    pub fn collect_tools(
        &self,
        provider: Arc<dyn querymt::LLMProvider>,
        runtime: Option<&crate::agent::core::SessionRuntime>,
        tool_config_override: Option<&crate::agent::core::ToolConfig>,
    ) -> Vec<querymt::chat::Tool> {
        let mut tools = Vec::new();
        let config = tool_config_override.unwrap_or(&self.tool_config);

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

        // Collect MCP tools (always when a runtime is present, regardless of
        // policy, because the policy was already chosen with MCP in mind in
        // builder_from_config).
        let mcp_tool_defs: Vec<querymt::chat::Tool> =
            if let (Some(runtime), ToolPolicy::ProviderOnly | ToolPolicy::BuiltInAndProvider) =
                (runtime, config.policy)
            {
                runtime.mcp_tool_state.tool_defs.read().unwrap().clone()
            } else {
                vec![]
            };

        // Filter built-in / provider tools with the basic allowlist check.
        let mut filtered: Vec<querymt::chat::Tool> = tools
            .into_iter()
            .filter(|tool| crate::agent::tools::is_tool_allowed_with(config, &tool.function.name))
            .collect();

        // Filter MCP tools with the server-name-aware check so that
        // "servername.*" wildcard entries in the allowlist are honoured.
        if let Some(runtime) = runtime {
            let mcp_tools = runtime.mcp_tool_state.tools.read().unwrap();
            for tool_def in mcp_tool_defs {
                let server_name = mcp_tools
                    .get(&tool_def.function.name)
                    .map(|a| a.server_name());
                if crate::agent::tools::is_mcp_tool_allowed_with(
                    config,
                    &tool_def.function.name,
                    server_name,
                ) {
                    filtered.push(tool_def);
                }
            }
        }

        filtered
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::helpers::empty_plugin_registry;
    use querymt::LLMParams;
    use std::sync::Arc;

    async fn make_config() -> (Arc<AgentConfig>, tempfile::TempDir) {
        use crate::session::backend::StorageBackend;
        let (registry, temp_dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock-model");
        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(registry),
                storage.session_store(),
                storage.event_journal(),
                llm,
            )
            .build(),
        );
        (config, temp_dir)
    }

    #[tokio::test]
    async fn is_tool_allowed_defaults_all_allowed() {
        let (config, _td) = make_config().await;
        assert!(config.is_tool_allowed("shell"));
        assert!(config.is_tool_allowed("read_tool"));
        assert!(config.is_tool_allowed("unknown_tool"));
    }

    #[tokio::test]
    async fn is_tool_allowed_with_denylist() {
        let (registry, _temp_dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock");
        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(registry),
                storage.session_store(),
                storage.event_journal(),
                llm,
            )
            .with_denied_tools(["shell".to_string()])
            .build(),
        );
        assert!(!config.is_tool_allowed("shell"));
        assert!(config.is_tool_allowed("read_tool"));
    }

    #[tokio::test]
    async fn is_tool_allowed_with_allowlist() {
        let (registry, _temp_dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock");
        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(registry),
                storage.session_store(),
                storage.event_journal(),
                llm,
            )
            .with_allowed_tools(["read_tool", "shell"])
            .build(),
        );
        assert!(config.is_tool_allowed("read_tool"));
        assert!(config.is_tool_allowed("shell"));
        assert!(!config.is_tool_allowed("delete_file"));
    }

    #[tokio::test]
    async fn tool_config_snapshot_returns_current_config() {
        let (config, _td) = make_config().await;
        let snapshot = config.tool_config_snapshot();
        // Default ToolPolicy is BuiltInAndProvider (see #[default] in core.rs)
        assert_eq!(snapshot.policy, ToolPolicy::BuiltInAndProvider);
    }

    #[tokio::test]
    async fn event_sink_returns_shared_instance() {
        let (config, _td) = make_config().await;
        // Verify event_sink is accessible and returns consistent fanout
        let _rx = config.subscribe_events();
    }

    #[tokio::test]
    async fn subscribe_events_returns_receiver() {
        let (config, _td) = make_config().await;
        let _rx = config.subscribe_events();
    }

    #[tokio::test]
    async fn agent_registry_returns_instance() {
        let (config, _td) = make_config().await;
        let registry = config.agent_registry();
        let agents = registry.list_agents();
        // Default empty registry
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn build_delegation_meta_empty_registry_returns_none() {
        let (config, _td) = make_config().await;
        let meta = config.build_delegation_meta();
        assert!(meta.is_none());
    }

    #[tokio::test]
    async fn requires_permission_for_tool_edit_tools() {
        let (config, _td) = make_config().await;
        // write_file and apply_patch → ToolKind::Edit, delete_file → Delete, shell → Execute
        assert!(config.requires_permission_for_tool("write_file"));
        assert!(config.requires_permission_for_tool("apply_patch"));
        assert!(config.requires_permission_for_tool("delete_file"));
        assert!(config.requires_permission_for_tool("shell"));
        // read-only tools do NOT require permission
        assert!(!config.requires_permission_for_tool("search_text"));
    }

    #[tokio::test]
    async fn should_snapshot_tool_defaults_mutating_all() {
        let (config, _td) = make_config().await;
        // Default assume_mutating=true means all tools trigger snapshot
        assert!(config.should_snapshot_tool("read_tool"));
        assert!(config.should_snapshot_tool("shell"));
    }

    #[tokio::test]
    async fn execution_config_snapshot_has_defaults() {
        let (config, _td) = make_config().await;
        let snapshot = config.execution_config_snapshot();
        assert_eq!(snapshot.execution_timeout_secs, 300);
        assert!(snapshot.max_steps.is_none());
    }

    #[tokio::test]
    async fn create_driver_with_no_steps() {
        let (config, _td) = make_config().await;
        let _driver = config.create_driver();
    }

    #[tokio::test]
    async fn create_driver_with_max_steps() {
        let (registry, _temp_dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock");
        let config = Arc::new(
            AgentConfigBuilder::new(
                Arc::new(registry),
                storage.session_store(),
                storage.event_journal(),
                llm,
            )
            .with_max_steps(100)
            .build(),
        );
        let _driver = config.create_driver();
        let limits = config.get_session_limits();
        assert!(limits.is_some());
    }

    #[tokio::test]
    async fn prepare_snapshot_no_cwd_returns_none() {
        let (config, _td) = make_config().await;
        let result = config.prepare_snapshot(None);
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn prepare_snapshot_policy_none_returns_none_even_with_cwd() {
        let (config, _td) = make_config().await;
        let result = config.prepare_snapshot(Some(std::path::Path::new("/tmp")));
        // Default policy is None
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn workspace_manager_actor_returns_ref() {
        let (config, _td) = make_config().await;
        let _actor = config.workspace_manager_actor();
    }

    #[tokio::test]
    async fn pending_elicitations_returns_map() {
        let (config, _td) = make_config().await;
        let map = config.pending_elicitations();
        let locked = map.try_lock().unwrap();
        assert!(locked.is_empty());
    }
}
