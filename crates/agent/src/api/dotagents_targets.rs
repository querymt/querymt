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
    /// Skills configuration of the registering runtime and its project root.
    /// Targets get a non-interactive skill catalog built from it.
    skills: Option<(crate::config::SkillsConfig, std::path::PathBuf)>,
}

impl DotagentsTargetFactory {
    pub fn new(plugin_registry: Arc<PluginRegistry>, storage: Arc<dyn StorageBackend>) -> Self {
        Self {
            plugin_registry,
            storage,
            skills: None,
        }
    }

    /// Give targets skills from the registering runtime's configuration.
    ///
    /// The catalog is non-interactive: a target's sessions have their own
    /// event sink with no fanout to the parent, so an approval prompt would
    /// never reach the user. Skills that need approval fail at once, naming
    /// the permission pattern to configure.
    pub fn with_skills(
        mut self,
        skills: Option<crate::config::SkillsConfig>,
        project_root: impl Into<std::path::PathBuf>,
    ) -> Self {
        self.skills = skills.map(|skills| (skills, project_root.into()));
        self
    }

    /// Whether a target with this `tools` list is offered the skill tool:
    /// skills are enabled and `tools` is absent, empty (unrestricted) or
    /// names `skill`.
    fn offers_skills(&self, plan: &DotagentsSubAgentPlan) -> bool {
        let Some((skills, _)) = &self.skills else {
            return false;
        };
        if !skills.enabled {
            return false;
        }
        match &plan.tools {
            None => true,
            Some(tools) if tools.is_empty() => true,
            Some(tools) => tools.iter().any(|tool| {
                tool.parse::<crate::config::ToolSpec>()
                    .is_ok_and(|spec| spec.is_builtin(crate::skills::SkillTool::NAME))
            }),
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
            let specs: Vec<crate::config::ToolSpec> = tools
                .iter()
                .filter_map(|tool| match tool.parse() {
                    Ok(spec) => Some(spec),
                    Err(error) => {
                        log::warn!("dotagents: ignoring tool `{tool}`: {error}");
                        None
                    }
                })
                .collect();
            // Servers attached or selected (`server.*`, bare name) keep MCP
            // tools visible; otherwise built-ins only.
            let policy =
                crate::agent::tools::infer_restricted_tool_policy(&specs, &plan.mcp_servers);
            builder = builder.with_tool_policy(policy).with_allowed_tools(specs);
        }
        if !plan.mcp_servers.is_empty() {
            builder = builder.with_mcp_servers(plan.mcp_servers.clone());
        }

        if self.offers_skills(plan)
            && let Some((skills, project_root)) = &self.skills
        {
            let catalog = crate::skills::SkillCatalog::from_config(skills, project_root);
            let catalog = Arc::new(
                Arc::try_unwrap(catalog)
                    .unwrap_or_else(|_| unreachable!("a fresh catalog has one owner"))
                    .with_approval(crate::skills::ApprovalMode::NonInteractive),
            );
            builder = builder.with_skill_catalog(catalog);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SkillsConfig;
    use crate::dotagents::{DotagentsLayer, DotagentsSource};
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::skills::{ApprovalMode, SkillTool};
    use crate::test_utils::mcp_harness::{HarnessOptions, McpHarness, ScriptedSkill};
    use crate::tools::{Tool, ToolContext, ToolError};

    fn plan(id: &str, tools: Option<Vec<&str>>) -> DotagentsSubAgentPlan {
        DotagentsSubAgentPlan {
            id: id.into(),
            info: AgentInfo {
                isolation: Default::default(),
                id: id.into(),
                name: id.into(),
                description: "d".into(),
                capabilities: vec![],
                required_capabilities: vec![],
                meta: None,
            },
            system_prompt: String::new(),
            model: None,
            tools: tools.map(|t| t.into_iter().map(String::from).collect()),
            mcp_servers: vec![],
            source: DotagentsSource::entry(DotagentsLayer::Workspace, "a.md", id),
            diagnostics: vec![],
        }
    }

    async fn factory(skills: Option<SkillsConfig>) -> (DotagentsTargetFactory, tempfile::TempDir) {
        let (registry, dir) = crate::test_utils::helpers::empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        (
            DotagentsTargetFactory::new(Arc::new(registry), storage)
                .with_skills(skills, dir.path()),
            dir,
        )
    }

    fn handle_of(
        f: &DotagentsTargetFactory,
        plan: &DotagentsSubAgentPlan,
    ) -> Arc<LocalAgentHandle> {
        let handle = f.create(plan).expect("target is created");
        let local = handle
            .as_any()
            .downcast_ref::<LocalAgentHandle>()
            .expect("local handle");
        Arc::new(LocalAgentHandle::from_config(local.config.clone()))
    }

    fn catalog_of(h: &LocalAgentHandle) -> Option<Arc<crate::skills::SkillCatalog>> {
        h.config.skill_catalog.clone()
    }

    #[tokio::test]
    async fn unrestricted_target_offers_skills_non_interactively() {
        let (f, _d) = factory(Some(SkillsConfig::default())).await;
        for tools in [None, Some(vec![]), Some(vec!["skill", "read_tool"])] {
            let h = handle_of(&f, &plan("t", tools.clone()));
            let catalog = catalog_of(&h).unwrap_or_else(|| panic!("catalog for {tools:?}"));
            assert_eq!(catalog.approval(), ApprovalMode::NonInteractive);
        }
    }

    #[tokio::test]
    async fn restricted_target_without_skill_gets_no_catalog() {
        let (f, _d) = factory(Some(SkillsConfig::default())).await;
        let h = handle_of(&f, &plan("t", Some(vec!["read_tool"])));
        assert!(catalog_of(&h).is_none());
    }

    #[tokio::test]
    async fn parent_without_skills_yields_no_catalog() {
        let (f, _d) = factory(None).await;
        assert!(catalog_of(&handle_of(&f, &plan("t", None))).is_none());
        let disabled = SkillsConfig {
            enabled: false,
            ..SkillsConfig::default()
        };
        let (f, _d) = factory(Some(disabled)).await;
        assert!(catalog_of(&handle_of(&f, &plan("t", None))).is_none());
    }

    #[tokio::test]
    async fn quorum_style_target_applies_the_registering_permissions() {
        let mut skills = SkillsConfig::default();
        skills
            .permissions
            .patterns
            .insert("docs.*".into(), crate::skills::PermissionLevel::Allow);
        let (f, _d) = factory(Some(skills)).await;
        let catalog = catalog_of(&handle_of(&f, &plan("t", None))).unwrap();
        assert_eq!(
            catalog.permissions().check_mcp("docs", "x"),
            crate::skills::PermissionLevel::Allow
        );
        assert_eq!(
            catalog.permissions().check_mcp("ops", "x"),
            crate::skills::PermissionLevel::Ask
        );
    }

    struct NoPromptContext;

    #[async_trait::async_trait]
    impl ToolContext for NoPromptContext {
        fn session_id(&self) -> &str {
            "s"
        }
        fn cwd(&self) -> Option<&std::path::Path> {
            None
        }
        async fn record_progress(
            &self,
            _: &str,
            _: String,
            _: Option<serde_json::Value>,
        ) -> Result<String, crate::tools::ToolError> {
            Ok(String::new())
        }
        async fn ask_question(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[(String, String)],
            _: bool,
        ) -> Result<Vec<String>, ToolError> {
            panic!("a target must never prompt");
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn target_loading_an_unconfigured_mcp_skill_fails_at_once() {
        let (f, _d) = factory(Some(SkillsConfig::default())).await;
        let h = handle_of(&f, &plan("t", None));
        let catalog = catalog_of(&h).unwrap();
        let mcp = McpHarness::start(HarnessOptions {
            skills: Some(vec![ScriptedSkill::new("a", "A")]),
            server_name: "docs",
            ..HarnessOptions::default()
        })
        .await;
        let ws = std::path::PathBuf::from(".");
        let tool = catalog.bind(Some(&ws), mcp.tool_state.clone(), Default::default());
        let err = tool
            .call(serde_json::json!({"name": "docs.a"}), &NoPromptContext)
            .await
            .unwrap_err();
        match err {
            ToolError::PermissionDenied(m) => assert!(m.contains("\"docs.*\" = \"allow\""), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(mcp.log.skills_get.lock().is_empty());
        assert!(h.config.pending_elicitations.lock().await.is_empty());
        let _ = SkillTool::NAME;
    }

    fn plan_with_servers(tools: Vec<&str>, servers: &[&str]) -> DotagentsSubAgentPlan {
        let mut p = plan("t", Some(tools));
        p.mcp_servers = servers
            .iter()
            .map(|name| crate::config::McpServerConfig::stdio(*name, "cmd"))
            .collect();
        p
    }

    #[tokio::test]
    async fn restricted_target_with_mcp_servers_keeps_only_listed_tools() {
        use crate::agent::core::ToolPolicy;
        use crate::config::ToolRef;
        let (f, _d) = factory(None).await;
        let h = handle_of(
            &f,
            &plan_with_servers(vec!["read_tool", "docs.*"], &["docs"]),
        );
        let config = &h.config.tool_config;
        assert_eq!(config.policy, ToolPolicy::BuiltInAndProvider);
        assert!(config.allows(ToolRef::builtin("read_tool")));
        assert!(config.allows(ToolRef::mcp("docs", "search")));
        assert!(!config.allows(ToolRef::builtin("shell")));
        assert!(!config.allows(ToolRef::mcp("ops", "search")));
    }

    #[tokio::test]
    async fn restricted_target_without_servers_is_builtin_only() {
        use crate::agent::core::ToolPolicy;
        use crate::config::ToolRef;
        let (f, _d) = factory(None).await;
        let h = handle_of(&f, &plan_with_servers(vec!["read_tool"], &[]));
        let config = &h.config.tool_config;
        assert_eq!(config.policy, ToolPolicy::BuiltInOnly);
        assert!(config.allows(ToolRef::builtin("read_tool")));
        assert!(!config.allows(ToolRef::builtin("shell")));
        assert!(!config.allows(ToolRef::mcp("docs", "search")));
    }
}
