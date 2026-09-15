//! Runtime factory for lazily materialized protocol delegation targets.
//!
//! This is the concrete [`ProtocolTargetFactory`] used by the standalone
//! (task 3.2) and quorum (task 3.3) build paths. It lives with the runtime
//! wiring because it needs the plugin registry and storage that the builder
//! already owns; the protocol module itself stays free of runtime construction.
//!
//! Child agents are built with `with_agent_registry_only` and an empty child
//! registry so a protocol target never recursively rediscovers the protocol
//! collection or reconciles inherited tasks (task 3.5).

use crate::agent::LocalAgentHandle;
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::core::ToolPolicy;
use crate::agent::handle::AgentHandle as AgentHandleTrait;
use crate::delegation::{AgentInfo, AgentRegistry};
use crate::dotagents::{DotagentsSubAgentPlan, ProtocolTargetFactory};
use crate::session::backend::StorageBackend;
use querymt::plugin::host::PluginRegistry;
use std::sync::Arc;

/// Builds protocol delegation targets using normal QueryMT runtime facilities.
///
/// Inherited settings (task 3.5): the parent's plugin registry, storage, and
/// tool-discovery facilities are shared, so a protocol target behaves like any
/// other delegate. Everything a target must **not** inherit is deliberately
/// excluded:
///
/// - The child gets an empty child registry via `with_agent_registry_only`,
///   which also skips `DelegateTool`/`DelegationMiddleware` auto-registration.
///   This is the recursion guard: a protocol target cannot rediscover the
///   protocol collection and cannot delegate onward into itself.
/// - No schedule repository, knowledge store, hooks, or middleware are attached,
///   so materializing a target never reconciles tasks or imports memories
///   owned by the parent.
///
/// `ProtocolTargetFactory::create` is called at most once per target by
/// [`crate::dotagents::DotagentsTargetRegistry`], which caches the resulting
/// handle. That, plus the child registry being empty, makes nested
/// materialization terminate deterministically.
pub struct DotagentsTargetFactory {
    plugin_registry: Arc<PluginRegistry>,
    storage: Arc<dyn StorageBackend>,
}

impl DotagentsTargetFactory {
    pub fn new(plugin_registry: Arc<PluginRegistry>, storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            plugin_registry,
            storage,
        }
    }
}

impl ProtocolTargetFactory for DotagentsTargetFactory {
    fn create(&self, plan: &DotagentsSubAgentPlan) -> Option<Arc<dyn AgentHandleTrait>> {
        let mut params = querymt::LLMParams::new();
        // The profile body is this target's agent-specific system prompt.
        // Frontmatter was already stripped by the parser.
        if !plan.system_prompt.trim().is_empty() {
            params = params.system(plan.system_prompt.clone());
        }
        // A preset failure was already diagnosed at plan time; fall back to the
        // base runtime rather than aborting delegation.
        if let Some(overlay) = &plan.model
            && overlay.apply_to(&mut params).is_err()
        {
            return None;
        }

        let mut builder =
            AgentConfigBuilder::new(self.plugin_registry.clone(), self.storage.clone(), params)
                .with_agent_id(plan.id.clone())
                // Recursion guard: an empty child registry and no DelegateTool or
                // DelegationMiddleware auto-registration means this target cannot
                // delegate onward or re-register the protocol collection.
                .with_agent_registry_only(Arc::new(ChildRegistry));

        if let Some(tools) = &plan.tools
            && !tools.is_empty()
        {
            builder = builder
                .with_tool_policy(ToolPolicy::BuiltInOnly)
                .with_allowed_tools(tools.clone());
        }
        if !plan.mcp_servers.is_empty() {
            builder = builder.with_mcp_servers(plan.mcp_servers.clone());
        }

        // Deliberately not inherited: schedule repository, knowledge store,
        // hooks, and middleware. A protocol target must not reconcile the
        // parent's tasks or re-import its memories when materialized.
        let config = Arc::new(builder.build());
        Some(Arc::new(LocalAgentHandle::from_config(config)) as Arc<dyn AgentHandleTrait>)
    }
}

/// Empty registry for child protocol targets.
///
/// Children reuse the parent's runtime facilities for model, tools, and MCP, but
/// they expose no delegation targets of their own: that is what stops protocol
/// sub-agents from recursively registering themselves (task 3.5).
#[derive(Debug, Default)]
struct ChildRegistry;

impl AgentRegistry for ChildRegistry {
    fn list_agents(&self) -> Vec<AgentInfo> {
        Vec::new()
    }

    fn get_agent(&self, _id: &str) -> Option<AgentInfo> {
        None
    }

    fn get_handle(&self, _id: &str) -> Option<Arc<dyn AgentHandleTrait>> {
        None
    }
}
