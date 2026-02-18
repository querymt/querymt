//! Builder for `AgentConfig`.
//!
//! `AgentConfigBuilder` is the direct replacement for the `QueryMTAgent` +
//! `AgentBuilderExt` pattern. It constructs an `AgentConfig` without going
//! through the old fat-struct intermediate.

use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{
    AgentMode, ClientState, DelegationContextConfig, DelegationContextTiming, SnapshotPolicy,
    ToolConfig, ToolPolicy,
};
use crate::config::{
    CompactionConfig, PruningConfig, RateLimitConfig, RuntimeExecutionPolicy, ToolOutputConfig,
};
use crate::delegation::{AgentRegistry, DefaultAgentRegistry};
use crate::event_bus::EventBus;
use crate::index::{WorkspaceIndexManagerActor, WorkspaceIndexManagerConfig};
use crate::middleware::{
    AgentModeMiddleware, ContextConfig, ContextMiddleware, DelegationConfig, DelegationMiddleware,
    LimitsConfig, LimitsMiddleware, MiddlewareDriver,
};
use crate::session::compaction::SessionCompaction;
use crate::session::provider::SessionProvider;
use crate::session::store::SessionStore;
use crate::tools::ToolRegistry;
use agent_client_protocol::AuthMethod;
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

/// Builder that constructs an [`AgentConfig`] directly.
///
/// Replaces the old `QueryMTAgent` + `AgentBuilderExt` pattern: instead of
/// building a fat intermediate struct that then copies ~25 fields into
/// `AgentConfig`, callers use this builder and call `build()` to get the
/// config directly.
pub struct AgentConfigBuilder {
    provider: Arc<SessionProvider>,
    default_mode: Arc<StdMutex<AgentMode>>,
    max_steps: Option<usize>,
    snapshot_policy: SnapshotPolicy,
    assume_mutating: bool,
    mutating_tools: HashSet<String>,
    max_prompt_bytes: Option<usize>,
    tool_config: ToolConfig,
    tool_registry: ToolRegistry,
    middleware_drivers: Vec<Arc<dyn MiddlewareDriver>>,
    event_bus: Arc<EventBus>,
    auth_methods: Vec<AuthMethod>,
    agent_registry: Arc<dyn AgentRegistry + Send + Sync>,
    delegation_context_config: DelegationContextConfig,
    workspace_manager_actor: kameo::actor::ActorRef<WorkspaceIndexManagerActor>,
    execution_timeout_secs: u64,
    execution_policy: RuntimeExecutionPolicy,
    compaction: SessionCompaction,
    snapshot_backend: Option<Arc<dyn crate::snapshot::SnapshotBackend>>,
    snapshot_gc_config: crate::snapshot::GcConfig,
    pending_elicitations: crate::elicitation::PendingElicitationMap,
}

impl AgentConfigBuilder {
    /// Create a new builder with the required infrastructure.
    ///
    /// Registers all built-in tools in the default tool registry.
    pub fn new(
        plugin_registry: Arc<PluginRegistry>,
        store: Arc<dyn SessionStore>,
        initial_config: LLMParams,
    ) -> Self {
        let provider = Arc::new(SessionProvider::new(plugin_registry, store, initial_config));
        let mut tool_registry = ToolRegistry::new();
        tool_registry.extend(crate::tools::builtins::all_builtin_tools());

        Self {
            provider,
            default_mode: Arc::new(StdMutex::new(AgentMode::Build)),
            max_steps: None,
            snapshot_policy: SnapshotPolicy::None,
            assume_mutating: true,
            mutating_tools: HashSet::new(),
            max_prompt_bytes: None,
            tool_config: ToolConfig::default(),
            tool_registry,
            middleware_drivers: Vec::new(),
            event_bus: Arc::new(EventBus::new()),
            auth_methods: Vec::new(),
            agent_registry: Arc::new(DefaultAgentRegistry::new()),
            delegation_context_config: DelegationContextConfig {
                timing: DelegationContextTiming::FirstTurnOnly,
                auto_inject: true,
            },
            workspace_manager_actor: WorkspaceIndexManagerActor::new(
                WorkspaceIndexManagerConfig::default(),
            ),
            execution_timeout_secs: 300,
            execution_policy: RuntimeExecutionPolicy::default(),
            compaction: SessionCompaction::new(),
            snapshot_backend: None,
            snapshot_gc_config: crate::snapshot::GcConfig::default(),
            pending_elicitations: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Build the final [`AgentConfig`].
    pub fn build(self) -> AgentConfig {
        AgentConfig {
            provider: self.provider,
            event_bus: self.event_bus,
            agent_registry: self.agent_registry,
            workspace_manager_actor: self.workspace_manager_actor,
            default_mode: self.default_mode,
            tool_config: self.tool_config,
            tool_registry: self.tool_registry,
            middleware_drivers: self.middleware_drivers,
            auth_methods: self.auth_methods,
            max_steps: self.max_steps,
            snapshot_policy: self.snapshot_policy,
            assume_mutating: self.assume_mutating,
            mutating_tools: self.mutating_tools,
            max_prompt_bytes: self.max_prompt_bytes,
            execution_timeout_secs: self.execution_timeout_secs,
            execution_policy: self.execution_policy,
            compaction: self.compaction,
            snapshot_backend: self.snapshot_backend,
            snapshot_gc_config: self.snapshot_gc_config,
            delegation_context_config: self.delegation_context_config,
            pending_elicitations: self.pending_elicitations,
        }
    }

    // ── Observer ─────────────────────────────────────────────────────────

    /// Add an event observer (non-consuming; useful during construction).
    pub fn add_observer(&self, observer: Arc<dyn crate::events::EventObserver>) {
        self.event_bus.add_observer(observer);
    }

    // ── Delegation ────────────────────────────────────────────────────────

    /// Sets the agent registry for delegation functionality.
    ///
    /// Also auto-registers `DelegationMiddleware` when the registry is non-empty
    /// and `auto_inject` is enabled.
    pub fn with_agent_registry(mut self, registry: Arc<dyn AgentRegistry + Send + Sync>) -> Self {
        self.agent_registry = registry.clone();

        // Remove old DelegateTool and add new one with registry
        self.tool_registry.remove("delegate");
        self.tool_registry
            .add(Arc::new(crate::tools::builtins::DelegateTool::new()));

        if self.delegation_context_config.auto_inject && !registry.list_agents().is_empty() {
            let middleware = DelegationMiddleware::new(
                self.provider.history_store(),
                registry,
                DelegationConfig {
                    context_timing: self.delegation_context_config.timing,
                    prevent_duplicates: false,
                    auto_inject: true,
                },
            );
            self.middleware_drivers.push(Arc::new(middleware));
        }

        self
    }

    /// Sets the delegation context timing.
    pub fn with_delegation_context_timing(mut self, timing: DelegationContextTiming) -> Self {
        self.delegation_context_config.timing = timing;
        self
    }

    /// Enables or disables auto delegation context injection.
    pub fn with_auto_delegation_context(mut self, enabled: bool) -> Self {
        self.delegation_context_config.auto_inject = enabled;
        self
    }

    /// Sets the delegation context config directly.
    pub fn with_delegation_context_config(mut self, config: DelegationContextConfig) -> Self {
        self.delegation_context_config = config;
        self
    }

    // ── Execution limits ──────────────────────────────────────────────────

    /// Sets the maximum number of execution steps.
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = Some(max_steps);
        self
    }

    /// Sets the maximum prompt size in bytes.
    pub fn with_max_prompt_bytes(mut self, bytes: usize) -> Self {
        self.max_prompt_bytes = Some(bytes);
        self
    }

    // ── Middleware ─────────────────────────────────────────────────────────

    /// Adds limits middleware.
    pub fn with_limits(mut self, config: LimitsConfig) -> Self {
        self.middleware_drivers
            .push(Arc::new(LimitsMiddleware::new(config)));
        self
    }

    /// Adds context management middleware.
    pub fn with_context_management(mut self, config: ContextConfig) -> Self {
        self.middleware_drivers
            .push(Arc::new(ContextMiddleware::new(config)));
        self
    }

    /// Adds delegation middleware.
    pub fn with_delegation(mut self, config: DelegationConfig) -> Self {
        let middleware = DelegationMiddleware::new(
            self.provider.history_store(),
            self.agent_registry.clone(),
            config,
        );
        self.middleware_drivers.push(Arc::new(middleware));
        self
    }

    /// Adds a single middleware.
    pub fn with_middleware<M: MiddlewareDriver + 'static>(mut self, middleware: M) -> Self {
        self.middleware_drivers.push(Arc::new(middleware));
        self
    }

    /// Adds multiple middlewares.
    pub fn with_middlewares(mut self, middlewares: Vec<Arc<dyn MiddlewareDriver>>) -> Self {
        self.middleware_drivers.extend(middlewares);
        self
    }

    /// Adds agent mode middleware with a custom plan-mode reminder.
    pub fn with_agent_mode_middleware<T: Into<String>>(mut self, reminder: T) -> Self {
        self.middleware_drivers
            .push(Arc::new(AgentModeMiddleware::new(reminder.into())));
        self
    }

    // ── Events ────────────────────────────────────────────────────────────

    /// Wires a shared event bus for aggregated event streaming.
    pub fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = bus;
        self
    }

    /// Adds an event observer (consuming).
    pub fn with_event_observer<O: crate::events::EventObserver + 'static>(
        self,
        observer: O,
    ) -> Self {
        self.event_bus.add_observer(Arc::new(observer));
        self
    }

    /// Sets the event observers (replaces any pending add calls, just adds them).
    pub fn with_event_observers(
        self,
        observers: Vec<Arc<dyn crate::events::EventObserver>>,
    ) -> Self {
        self.event_bus.add_observers(observers);
        self
    }

    // ── Auth ──────────────────────────────────────────────────────────────

    /// Sets authentication methods.
    pub fn with_auth_methods<I>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = AuthMethod>,
    {
        self.auth_methods = methods.into_iter().collect();
        self
    }

    /// Sets the client state.
    pub fn with_client_state(self, _state: ClientState) -> Self {
        // ClientState lives on AgentHandle (connection level), not AgentConfig.
        // Accept the call for API compatibility but do nothing here.
        self
    }

    // ── Snapshot ──────────────────────────────────────────────────────────

    /// Sets the snapshot policy.
    pub fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = policy;
        self
    }

    /// Sets the snapshot backend for undo/redo support.
    pub fn with_snapshot_backend(
        mut self,
        backend: Arc<dyn crate::snapshot::SnapshotBackend>,
    ) -> Self {
        self.snapshot_backend = Some(backend);
        self
    }

    /// Sets the snapshot GC configuration.
    pub fn with_snapshot_gc_config(mut self, config: crate::snapshot::GcConfig) -> Self {
        self.snapshot_gc_config = config;
        self
    }

    // ── Tools ─────────────────────────────────────────────────────────────

    /// Sets the tool policy.
    pub fn with_tool_policy(mut self, policy: ToolPolicy) -> Self {
        self.tool_config.policy = policy;
        self
    }

    /// Sets the tool configuration.
    pub fn with_tool_config(mut self, config: ToolConfig) -> Self {
        self.tool_config = config;
        self
    }

    /// Sets the tool registry (replaces the default one).
    pub fn with_tool_registry(mut self, registry: ToolRegistry) -> Self {
        self.tool_registry = registry;
        self
    }

    /// Sets the allowed tools allowlist.
    pub fn with_allowed_tools<I, S>(mut self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tool_config.allowlist = Some(
            tool_names
                .into_iter()
                .map(Into::into)
                .collect::<HashSet<_>>(),
        );
        self
    }

    /// Sets the denied tools denylist.
    pub fn with_denied_tools<I, S>(mut self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tool_config.denylist = tool_names.into_iter().map(Into::into).collect();
        self
    }

    /// Sets specific tools to be considered mutating.
    pub fn with_mutating_tools<I>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        self.mutating_tools = tools.into_iter().collect();
        self
    }

    /// Sets whether to assume all tools are mutating by default.
    pub fn with_assume_mutating(mut self, assume: bool) -> Self {
        self.assume_mutating = assume;
        self
    }

    // ── Compaction ────────────────────────────────────────────────────────

    /// Sets the full execution policy at once (tool output, pruning, compaction, rate limit).
    pub fn with_execution_policy(mut self, policy: RuntimeExecutionPolicy) -> Self {
        self.execution_policy = policy;
        self
    }

    /// Sets the tool output truncation configuration (Layer 1).
    pub fn with_tool_output_config(mut self, config: ToolOutputConfig) -> Self {
        self.execution_policy.tool_output = config;
        self
    }

    /// Sets the pruning configuration (Layer 2).
    pub fn with_pruning_config(mut self, config: PruningConfig) -> Self {
        self.execution_policy.pruning = config;
        self
    }

    /// Sets the AI compaction configuration (Layer 3).
    pub fn with_compaction_config(mut self, config: CompactionConfig) -> Self {
        self.execution_policy.compaction = config;
        self
    }

    /// Enables full compaction with default settings.
    pub fn with_compaction_enabled(self) -> Self {
        self.with_pruning_config(PruningConfig::default())
            .with_compaction_config(CompactionConfig::default())
    }

    // ── Rate limiting ─────────────────────────────────────────────────────

    /// Sets the rate limit retry configuration.
    pub fn with_rate_limit_config(mut self, config: RateLimitConfig) -> Self {
        self.execution_policy.rate_limit = config;
        self
    }

    // ── Mode ──────────────────────────────────────────────────────────────

    /// Sets the initial agent mode.
    pub fn with_agent_mode(self, mode: AgentMode) -> Self {
        if let Ok(mut default_mode) = self.default_mode.lock() {
            *default_mode = mode;
        }
        self
    }

    // ── Internal accessors (used by simple::agent and quorum) ─────────────

    /// Read the current `event_bus` (for passing to middleware factories).
    pub fn event_bus(&self) -> Arc<EventBus> {
        self.event_bus.clone()
    }

    /// Read the current `compaction_config` (for auto-ContextMiddleware check).
    pub fn compaction_config(&self) -> &CompactionConfig {
        &self.execution_policy.compaction
    }

    /// Read the current `default_mode` (for AgentHandle construction).
    pub fn default_mode_value(&self) -> AgentMode {
        self.default_mode
            .lock()
            .map(|m| *m)
            .unwrap_or(AgentMode::Build)
    }

    /// Extend the tool registry with additional tools.
    pub fn extend_tool_registry(
        &mut self,
        tools: impl IntoIterator<Item = Arc<dyn crate::tools::Tool>>,
    ) {
        for tool in tools {
            self.tool_registry.add(tool);
        }
    }

    /// Mutably access the tool registry (for adding tools during build).
    pub fn tool_registry_mut(&mut self) -> &mut ToolRegistry {
        &mut self.tool_registry
    }

    /// Mutably push a middleware driver (for quorum helper function).
    pub fn push_middleware(&mut self, driver: Arc<dyn MiddlewareDriver>) {
        self.middleware_drivers.push(driver);
    }

    /// Borrow the `SessionProvider` (used by `DelegationMiddleware` constructor in quorum).
    pub fn provider(&self) -> &Arc<SessionProvider> {
        &self.provider
    }

    /// Borrow the `agent_registry` (used by `DelegationMiddleware` constructor in quorum).
    pub fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.agent_registry.clone()
    }

    /// Borrow the `delegation_context_config` (used when building DelegationMiddleware).
    pub fn delegation_context_config(&self) -> &DelegationContextConfig {
        &self.delegation_context_config
    }
}
