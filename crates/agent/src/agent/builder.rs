//! Builder pattern methods for QueryMTAgent configuration

use crate::agent::core::{
    ClientState, DelegationContextConfig, DelegationContextTiming, QueryMTAgent, SnapshotPolicy,
    ToolConfig, ToolPolicy,
};
use crate::config::{CompactionConfig, PruningConfig, ToolOutputConfig};
use crate::delegation::AgentRegistry;
use crate::event_bus::EventBus;
use crate::middleware::{
    ContextConfig, ContextMiddleware, DelegationConfig, DelegationMiddleware, LimitsConfig,
    LimitsMiddleware, PlanModeMiddleware,
};
use crate::tools::ToolRegistry;
use agent_client_protocol::AuthMethod;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// Extension trait providing builder pattern methods for QueryMTAgent
pub trait AgentBuilderExt {
    /// Sets the agent registry for delegation functionality.
    fn with_agent_registry(self, registry: Arc<dyn AgentRegistry + Send + Sync>) -> Self;

    /// Sets the delegation context timing.
    fn with_delegation_context_timing(self, timing: DelegationContextTiming) -> Self;

    /// Enables or disables auto delegation context injection.
    fn with_auto_delegation_context(self, enabled: bool) -> Self;

    /// Sets the maximum number of execution steps.
    fn with_max_steps(self, max_steps: usize) -> Self;

    /// Adds limits middleware with combined configuration.
    fn with_limits(self, config: LimitsConfig) -> Self;

    /// Adds context management middleware.
    fn with_context_management(self, config: ContextConfig) -> Self;

    /// Adds delegation middleware with combined configuration.
    fn with_delegation(self, config: DelegationConfig) -> Self;

    /// Adds a middleware to the agent.
    fn with_middleware<M: crate::middleware::MiddlewareDriver + 'static>(
        self,
        middleware: M,
    ) -> Self;

    /// Adds multiple middlewares to the agent.
    fn with_middlewares(
        self,
        middlewares: Vec<Arc<dyn crate::middleware::MiddlewareDriver>>,
    ) -> Self;

    /// Adds a middleware preset collection.
    fn with_middleware_preset(
        self,
        middlewares: Vec<Arc<dyn crate::middleware::MiddlewareDriver>>,
    ) -> Self;

    /// Adds plan mode middleware with a custom reminder.
    fn with_plan_mode_middleware<T: Into<String>>(self, reminder: T) -> Self;

    /// Adds an event observer to the agent.
    fn with_event_observer<O: crate::events::EventObserver + 'static>(self, observer: O) -> Self;

    /// Wires a shared event bus for aggregated event streaming.
    fn with_event_bus(self, bus: Arc<EventBus>) -> Self;

    /// Sets authentication methods for the agent.
    fn with_auth_methods<I>(self, methods: I) -> Self
    where
        I: IntoIterator<Item = AuthMethod>;

    /// Sets the snapshot root directory.
    fn with_snapshot_root(self, path: std::path::PathBuf) -> Self;

    /// Sets the snapshot policy.
    fn with_snapshot_policy(self, policy: SnapshotPolicy) -> Self;

    /// Sets the tool policy.
    fn with_tool_policy(self, policy: ToolPolicy) -> Self;

    /// Sets the maximum prompt size in bytes.
    fn with_max_prompt_bytes(self, bytes: usize) -> Self;

    /// Sets specific tools to be considered mutating.
    fn with_mutating_tools<I>(self, tools: I) -> Self
    where
        I: IntoIterator<Item = String>;

    /// Sets whether to assume all tools are mutating by default.
    fn with_assume_mutating(self, assume: bool) -> Self;

    /// Sets the tool configuration.
    fn with_tool_config(self, config: ToolConfig) -> Self;

    /// Sets the tool registry.
    fn with_tool_registry(self, registry: ToolRegistry) -> Self;

    /// Sets the allowed tools whitelist.
    fn with_allowed_tools<I, S>(self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>;

    /// Sets the denied tools blacklist.
    fn with_denied_tools<I, S>(self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>;

    /// Sets the plan mode enabled flag.
    fn with_plan_mode_enabled(self, enabled: bool) -> Self;

    /// Sets the event observers.
    fn with_event_observers(self, observers: Vec<Arc<dyn crate::events::EventObserver>>) -> Self;

    /// Sets the client state.
    fn with_client_state(self, state: ClientState) -> Self;

    /// Sets the auth methods.
    fn with_auth_methods_arc(self, methods: Arc<StdMutex<Vec<AuthMethod>>>) -> Self;

    /// Sets the client.
    fn with_client(self, client: Arc<dyn agent_client_protocol::Client + Send + Sync>) -> Self;

    /// Sets the agent registry (internal trait).
    fn with_agent_registry_arc(self, registry: Arc<dyn AgentRegistry + Send + Sync>) -> Self;

    /// Sets the delegation context config.
    fn with_delegation_context_config(self, config: DelegationContextConfig) -> Self;

    // Compaction system configuration (3-layer)

    /// Sets the tool output truncation configuration (Layer 1)
    fn with_tool_output_config(self, config: ToolOutputConfig) -> Self;

    /// Sets the pruning configuration (Layer 2)
    fn with_pruning_config(self, config: PruningConfig) -> Self;

    /// Sets the AI compaction configuration (Layer 3)
    fn with_compaction_config(self, config: CompactionConfig) -> Self;

    /// Enables full compaction with default settings
    /// This enables pruning and auto-compaction, and adds ContextMiddleware if needed
    fn with_compaction_enabled(self) -> Self;

    /// Sets the snapshot backend for undo/redo support.
    fn with_snapshot_backend(self, backend: Arc<dyn crate::snapshot::SnapshotBackend>) -> Self;

    /// Sets the snapshot GC configuration.
    fn with_snapshot_gc_config(self, config: crate::snapshot::GcConfig) -> Self;
}

impl AgentBuilderExt for QueryMTAgent {
    fn with_agent_registry(mut self, registry: Arc<dyn AgentRegistry + Send + Sync>) -> Self {
        self.agent_registry = registry.clone();

        // Update DelegateTool with registry for validation and enum support
        if let Ok(mut tool_reg) = self.tool_registry.lock() {
            // Remove old DelegateTool and add new one with registry
            tool_reg.remove("delegate");
            tool_reg.add(Arc::new(crate::tools::builtins::DelegateTool::new()));
        }

        // Auto-register delegation middleware if enabled and registry is non-empty
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
            self.middleware_drivers
                .lock()
                .unwrap()
                .push(Arc::new(middleware));
        }

        self
    }

    fn with_delegation_context_timing(mut self, timing: DelegationContextTiming) -> Self {
        self.delegation_context_config.timing = timing;
        self
    }

    fn with_auto_delegation_context(mut self, enabled: bool) -> Self {
        self.delegation_context_config.auto_inject = enabled;
        self
    }

    fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = Some(max_steps);
        self
    }

    fn with_limits(self, config: LimitsConfig) -> Self {
        let middleware = LimitsMiddleware::new(config);
        self.middleware_drivers
            .lock()
            .unwrap()
            .push(Arc::new(middleware));
        self
    }

    fn with_context_management(self, config: ContextConfig) -> Self {
        let middleware = ContextMiddleware::new(config);
        self.middleware_drivers
            .lock()
            .unwrap()
            .push(Arc::new(middleware));
        self
    }

    fn with_delegation(self, config: DelegationConfig) -> Self {
        let middleware = DelegationMiddleware::new(
            self.provider.history_store(),
            self.agent_registry.clone(),
            config,
        );
        self.middleware_drivers
            .lock()
            .unwrap()
            .push(Arc::new(middleware));
        self
    }

    fn with_middleware<M: crate::middleware::MiddlewareDriver + 'static>(
        self,
        middleware: M,
    ) -> Self {
        self.middleware_drivers
            .lock()
            .unwrap()
            .push(Arc::new(middleware));
        self
    }

    fn with_middlewares(
        self,
        middlewares: Vec<Arc<dyn crate::middleware::MiddlewareDriver>>,
    ) -> Self {
        self.middleware_drivers.lock().unwrap().extend(middlewares);
        self
    }

    fn with_middleware_preset(
        self,
        middlewares: Vec<Arc<dyn crate::middleware::MiddlewareDriver>>,
    ) -> Self {
        self.middleware_drivers.lock().unwrap().extend(middlewares);
        self
    }

    fn with_plan_mode_middleware<T: Into<String>>(self, reminder: T) -> Self {
        let middleware = PlanModeMiddleware::new(self.plan_mode_enabled.clone(), reminder.into());
        self.middleware_drivers
            .lock()
            .unwrap()
            .push(Arc::new(middleware));
        self
    }

    fn with_event_observer<O: crate::events::EventObserver + 'static>(self, observer: O) -> Self {
        self.event_bus.add_observer(Arc::new(observer));
        self
    }

    fn with_event_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.event_bus = bus;
        self
    }

    fn with_auth_methods<I>(self, methods: I) -> Self
    where
        I: IntoIterator<Item = AuthMethod>,
    {
        *self.auth_methods.lock().unwrap() = methods.into_iter().collect();
        self
    }

    fn with_snapshot_root(mut self, path: std::path::PathBuf) -> Self {
        self.snapshot_root = Some(path);
        self
    }

    fn with_snapshot_policy(mut self, policy: SnapshotPolicy) -> Self {
        self.snapshot_policy = policy;
        self
    }

    fn with_tool_policy(self, policy: ToolPolicy) -> Self {
        self.tool_config.lock().unwrap().policy = policy;
        self
    }

    fn with_max_prompt_bytes(mut self, bytes: usize) -> Self {
        self.max_prompt_bytes = Some(bytes);
        self
    }

    fn with_mutating_tools<I>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        self.mutating_tools = tools.into_iter().collect();
        self
    }

    fn with_assume_mutating(mut self, assume: bool) -> Self {
        self.assume_mutating = assume;
        self
    }

    fn with_tool_config(self, config: ToolConfig) -> Self {
        *self.tool_config.lock().unwrap() = config;
        self
    }

    fn with_tool_registry(self, registry: ToolRegistry) -> Self {
        *self.tool_registry.lock().unwrap() = registry;
        self
    }

    fn with_allowed_tools<I, S>(self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let allow = tool_names
            .into_iter()
            .map(Into::into)
            .collect::<HashSet<_>>();
        if let Ok(mut config) = self.tool_config.lock() {
            config.allowlist = Some(allow);
        }
        self
    }

    fn with_denied_tools<I, S>(self, tool_names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Ok(mut config) = self.tool_config.lock() {
            config.denylist = tool_names.into_iter().map(Into::into).collect();
        }
        self
    }

    fn with_plan_mode_enabled(self, enabled: bool) -> Self {
        self.plan_mode_enabled
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
        self
    }

    fn with_event_observers(self, observers: Vec<Arc<dyn crate::events::EventObserver>>) -> Self {
        self.event_bus.add_observers(observers);
        self
    }

    fn with_client_state(self, state: ClientState) -> Self {
        *self.client_state.lock().unwrap() = Some(state);
        self
    }

    fn with_auth_methods_arc(mut self, methods: Arc<StdMutex<Vec<AuthMethod>>>) -> Self {
        self.auth_methods = methods;
        self
    }

    fn with_client(self, client: Arc<dyn agent_client_protocol::Client + Send + Sync>) -> Self {
        *self.client.lock().unwrap() = Some(client);
        self
    }

    fn with_agent_registry_arc(mut self, registry: Arc<dyn AgentRegistry + Send + Sync>) -> Self {
        self.agent_registry = registry;
        self
    }

    fn with_delegation_context_config(mut self, config: DelegationContextConfig) -> Self {
        self.delegation_context_config = config;
        self
    }

    // Compaction system configuration (3-layer)

    fn with_tool_output_config(mut self, config: ToolOutputConfig) -> Self {
        self.tool_output_config = config;
        self
    }

    fn with_pruning_config(mut self, config: PruningConfig) -> Self {
        self.pruning_config = config;
        self
    }

    fn with_compaction_config(mut self, config: CompactionConfig) -> Self {
        // If auto compaction is enabled, auto-enable ContextMiddleware
        if config.auto {
            let mut drivers = self.middleware_drivers.lock().unwrap();
            let already_has = drivers.iter().any(|d| d.name() == "ContextMiddleware");
            if !already_has {
                log::info!("Auto-enabling ContextMiddleware for compaction");
                let context_middleware = ContextMiddleware::new(ContextConfig::default());
                drivers.push(Arc::new(context_middleware));
            } else {
                log::debug!("ContextMiddleware already present, skipping auto-add");
            }
        }
        self.compaction_config = config;
        self
    }

    fn with_compaction_enabled(self) -> Self {
        // Enable pruning and compaction with defaults
        self.with_pruning_config(PruningConfig::default())
            .with_compaction_config(CompactionConfig::default())
    }

    fn with_snapshot_backend(mut self, backend: Arc<dyn crate::snapshot::SnapshotBackend>) -> Self {
        self.snapshot_backend = Some(backend);
        self
    }

    fn with_snapshot_gc_config(mut self, config: crate::snapshot::GcConfig) -> Self {
        self.snapshot_gc_config = config;
        self
    }
}

impl QueryMTAgent {
    /// Sets the tool policy.
    pub fn set_tool_policy(&self, policy: ToolPolicy) {
        if let Ok(mut config) = self.tool_config.lock() {
            config.policy = policy;
        }
    }

    /// Sets the allowed tools whitelist.
    pub fn set_allowed_tools<I, S>(&self, tool_names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Ok(mut config) = self.tool_config.lock() {
            config.allowlist = Some(tool_names.into_iter().map(Into::into).collect());
        }
    }

    /// Clears the allowed tools whitelist.
    pub fn clear_allowed_tools(&self) {
        if let Ok(mut config) = self.tool_config.lock() {
            config.allowlist = None;
        }
    }

    /// Sets the denied tools blacklist.
    pub fn set_denied_tools<I, S>(&self, tool_names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let Ok(mut config) = self.tool_config.lock() {
            config.denylist = tool_names.into_iter().map(Into::into).collect();
        }
    }

    /// Clears the denied tools blacklist.
    pub fn clear_denied_tools(&self) {
        if let Ok(mut config) = self.tool_config.lock() {
            config.denylist.clear();
        }
    }
}
