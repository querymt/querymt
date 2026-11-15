//! Building member agents.
//!
//! The description of a member ([`MemberBuildSpec`]), the helpers that turn a
//! tool list into specs and capabilities, and the function that configures a
//! local delegate's [`AgentConfigBuilder`]. These used to live in the top
//! package; they are here so the delegation machinery and the code that
//! builds its members share one package.

use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::builtin_tools::all_builtin_tools;
use crate::agent::core::{AgentMode, SnapshotPolicy, ToolPolicy};
use crate::config::{
    DelegationWaitPolicy, ExecutionPolicy, HooksConfig, McpServerConfig, MiddlewareEntry,
    Permissions, SkillsConfig, ToolSpec, ToolSpecError,
};
use crate::session::backend::StorageBackend;
use crate::tools::CapabilityRequirement;
use crate::worktree::DelegationIsolation;
use once_cell::sync::Lazy;
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt_agent_services::event_fanout::EventFanout;
use querymt_agent_services::snapshot::GitSnapshotBackend;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Parses tool selectors; keeps the valid ones and the first error.
pub fn parse_tool_specs<I, S>(tools: I) -> (Vec<ToolSpec>, Option<ToolSpecError>)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut specs = Vec::new();
    let mut error = None;
    for tool in tools {
        match tool.as_ref().parse() {
            Ok(spec) => specs.push(spec),
            Err(e) => {
                error.get_or_insert(e);
            }
        }
    }
    (specs, error)
}

/// Builtin tool names that require filesystem access, computed from the
/// builtin tool definitions so it stays in sync with them.
static FILESYSTEM_TOOLS: Lazy<HashSet<String>> = Lazy::new(|| {
    all_builtin_tools()
        .into_iter()
        .filter(|tool| {
            tool.required_capabilities()
                .contains(&CapabilityRequirement::Filesystem)
        })
        .map(|tool| tool.name().to_string())
        .collect()
});

fn tool_requires_filesystem(tool_name: &str) -> bool {
    FILESYSTEM_TOOLS.contains(tool_name)
}

/// Capabilities the given tool selectors need from the host.
pub fn infer_required_capabilities(tools: &[ToolSpec]) -> HashSet<CapabilityRequirement> {
    let mut caps = HashSet::new();
    for tool in tools {
        if tool.as_builtin().is_some_and(tool_requires_filesystem) {
            caps.insert(CapabilityRequirement::Filesystem);
        }
    }
    caps
}

/// Everything needed to build one member (or the planner) of a runtime.
#[doc(hidden)]
#[derive(Clone)]
pub struct MemberBuildSpec {
    pub id: String,
    pub llm_config: Option<LLMParams>,
    pub capabilities: Vec<String>,
    pub tools: Vec<ToolSpec>,
    /// First malformed selector passed to `tools(..)`; reported by `build()`.
    pub tool_error: Option<ToolSpecError>,
    pub description: Option<String>,
    pub required_capabilities: Vec<CapabilityRequirement>,
    pub middleware: Vec<MiddlewareEntry>,
    pub execution: ExecutionPolicy,
    pub skills: Option<SkillsConfig>,
    pub mode: AgentMode,
    pub permissions: Permissions,
    pub hooks: HooksConfig,
    /// MCP servers resolved for this agent, attached to its runtime through the
    /// existing stdio and streamable-http transports.
    pub mcp_servers: Vec<McpServerConfig>,
    /// Configured isolation for tasks delegated to this agent.
    pub isolation: DelegationIsolation,
}

impl MemberBuildSpec {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            llm_config: None,
            capabilities: Vec::new(),
            tools: Vec::new(),
            tool_error: None,
            description: None,
            required_capabilities: Vec::new(),
            middleware: Vec::new(),
            execution: ExecutionPolicy::default(),
            skills: None,
            mode: AgentMode::default(),
            permissions: Permissions::default(),
            hooks: HooksConfig::default(),
            mcp_servers: Vec::new(),
            isolation: DelegationIsolation::default(),
        }
    }

    /// The LLM parameters, or an error when `.provider()` was never called.
    pub fn llm_params(&self) -> anyhow::Result<LLMParams> {
        self.llm_config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("LLM configuration required (call .provider() first)"))
    }
}

/// What every delegate of one runtime shares.
#[doc(hidden)]
pub struct DelegateBuildParams {
    pub plugin_registry: Arc<PluginRegistry>,
    pub snapshot_policy: SnapshotPolicy,
    pub skills_project_root: PathBuf,
    pub wait_policy: DelegationWaitPolicy,
    pub wait_timeout_secs: u64,
    pub cancel_grace_secs: u64,
    pub workspace_service: Option<Arc<crate::worktree::WorkspaceService>>,
    pub repository_knowledge:
        Option<Arc<crate::knowledge::repository::RepositoryKnowledgeRegistry>>,
}

/// Configures the builder of one local delegate. The caller builds the handle.
#[doc(hidden)]
pub fn configure_delegate_builder(
    spec: &MemberBuildSpec,
    params: &DelegateBuildParams,
    storage: Arc<dyn StorageBackend>,
    event_fanout: Arc<EventFanout>,
) -> anyhow::Result<AgentConfigBuilder> {
    let tools = &spec.tools;
    let mut b =
        AgentConfigBuilder::new(params.plugin_registry.clone(), storage, spec.llm_params()?)
            .with_agent_id(spec.id.clone())
            .with_tool_policy(ToolPolicy::BuiltInOnly)
            .with_snapshot_policy(params.snapshot_policy)
            .with_event_fanout(event_fanout);

    if params.snapshot_policy != SnapshotPolicy::None {
        b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
    }

    if let Some(skills) = spec.skills.as_ref()
        && skills.enabled
        && (tools.is_empty()
            || tools
                .iter()
                .any(|tool| tool.is_builtin(crate::skills::SkillTool::NAME)))
    {
        b = b.with_skill_catalog(crate::skills::SkillCatalog::from_config(
            skills,
            &params.skills_project_root,
        ));
    }

    // Infer the policy like the single-agent path: any external tool spec,
    // resolved MCP selection or attached MCP server needs BuiltInAndProvider
    // so MCP tools and skills reach the LLM, even when the tool list is
    // empty; otherwise BuiltInOnly is sufficient.
    let policy = crate::agent::tools::infer_restricted_tool_policy(tools, &spec.mcp_servers);
    b = b.with_tool_policy(policy);
    if !tools.is_empty() {
        b = b.with_allowed_tools(tools.clone());
    }

    // Attach resolved MCP servers through the existing lifecycle.
    if !spec.mcp_servers.is_empty() {
        b = b.with_mcp_servers(spec.mcp_servers.clone());
    }

    b = b
        .with_agent_mode(spec.mode)
        .with_snapshot_from_execution(&spec.execution)
        .with_delegation_wait_policy(params.wait_policy.clone())
        .with_delegation_wait_timeout_secs(params.wait_timeout_secs)
        .with_delegation_cancel_grace_secs(params.cancel_grace_secs)
        .with_permissions(spec.permissions.clone());
    apply_middleware_from_config(&mut b, &spec.middleware)?;
    if let Some(service) = params.workspace_service.clone() {
        b = b.with_workspace_service(service);
    }
    b = b.with_repository_knowledge(params.repository_knowledge.clone());
    Ok(b)
}

/// Applies middleware from config entries (plus the shared defaults) to a builder.
///
/// Uses the same assembly path as `AgentBuilder::build`, so an invalid entry
/// fails identically for single agents, planners and delegates.
///
/// # Errors
///
/// Fails when an entry has an unknown type or invalid configuration.
#[doc(hidden)]
pub fn apply_middleware_from_config(
    builder: &mut AgentConfigBuilder,
    entries: &[MiddlewareEntry],
) -> anyhow::Result<()> {
    use crate::config::RuntimeExecutionPolicy;

    // Lightweight snapshot of the config built so far: factories only read the
    // compaction policy, event journal/sink and tool registry from it.
    let ephemeral_policy = RuntimeExecutionPolicy {
        compaction: builder.compaction_config().clone(),
        ..Default::default()
    };
    let factory_config = AgentConfigBuilder::from_provider(
        builder.storage().clone(),
        builder.provider().clone(),
        builder.event_journal(),
    )
    .with_agent_registry(builder.agent_registry())
    .with_execution_policy(ephemeral_policy)
    .with_tool_registry(builder.tool_registry_snapshot())
    .build();

    let existing = builder.middleware_snapshot();
    for driver in crate::middleware::build_from_entries(entries, &existing, &factory_config)? {
        builder.push_middleware(driver);
    }
    Ok(())
}
