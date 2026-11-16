//! Building member agents.
//!
//! The description of a member ([`MemberBuildSpec`]), the helpers that turn a
//! tool list into specs and capabilities, and the function that configures a
//! local delegate's [`AgentConfigBuilder`]. These used to live in the top
//! package; they are here so the delegation machinery and the code that
//! builds its members share one package.

use super::roster::{AgentId, RosterError};
use crate::agent::agent_config_builder::AgentConfigBuilder;
use crate::agent::builtin_tools::all_builtin_tools;
use crate::agent::core::{AgentMode, SnapshotPolicy, ToolPolicy};
use crate::config::{
    DelegateConfig, DelegationSettings, ExecutionPolicy, HooksConfig, McpServerConfig,
    MiddlewareEntry, Permissions, RemoteAgentConfig, SkillsConfig, ToolSpec, ToolSpecError,
};
use crate::delegation::AgentInfo;
use crate::session::backend::StorageBackend;
use crate::tools::CapabilityRequirement;
use crate::worktree::DelegationIsolation;
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt_agent_services::event_fanout::EventFanout;
use querymt_agent_services::snapshot::GitSnapshotBackend;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

/// The description of a member, used by TOML, Rust and ACP alike.
///
/// Input from outside the process (`querymt/agents/add`) is checked by
/// [`MemberSpec::check_external`] before it is lowered.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MemberSpec {
    Local(Box<DelegateConfig>),
    Remote(RemoteAgentConfig),
}

impl MemberSpec {
    /// The member's ID as written, before it is validated.
    pub fn raw_id(&self) -> &str {
        match self {
            Self::Local(delegate) => &delegate.id,
            Self::Remote(remote) => &remote.id,
        }
    }

    /// Rejects what a client must not be able to ask a runtime to do.
    ///
    /// - A system prompt part that names a file: the runtime would read it
    ///   on the client's behalf, and `resolved()` would otherwise drop it
    ///   without a word.
    ///
    /// `${VAR}` in `api_key` is not expanded for such input, so a client
    /// cannot send an environment secret to an endpoint it chose.
    pub fn check_external(&self, id: &AgentId) -> Result<(), RosterError> {
        if let Self::Local(delegate) = self
            && delegate.system.iter().any(|part| part.resolved().is_none())
        {
            return Err(RosterError::FilePromptPart { id: id.clone() });
        }
        Ok(())
    }
}

/// Names of the tools a local member may select: the built-in tools and `skill`.
pub fn available_local_tool_names() -> HashSet<String> {
    let mut names: HashSet<String> = crate::agent::builtin_tools::builtin_tool_names()
        .into_iter()
        .collect();
    names.insert(crate::skills::SkillTool::NAME.to_string());
    names
}

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
static FILESYSTEM_TOOLS: std::sync::LazyLock<HashSet<String>> = std::sync::LazyLock::new(|| {
    // `delegate` is not in `all_builtin_tools()` (it needs a roster), and it
    // needs no filesystem capability, so this list is complete.
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

/// Whether a member takes what its runtime configures for every member:
/// snapshot policy, workspace service, repository knowledge, delegation
/// settings, mode, permissions and middleware.
///
/// `.agents` protocol targets are [`Standalone`](Self::Standalone): they must
/// not reconcile the parent's tasks or re-import its memories when built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeScope {
    #[default]
    Inherit,
    Standalone,
}

/// Everything needed to build one member of a runtime.
#[doc(hidden)]
#[derive(Clone)]
pub struct MemberBuildSpec {
    pub id: AgentId,
    pub llm_config: Option<LLMParams>,
    pub capabilities: Vec<String>,
    pub tools: Vec<ToolSpec>,
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
    /// Tool policy, when it is not inferred from `tools` and `mcp_servers`.
    pub tool_policy: Option<ToolPolicy>,
    pub scope: RuntimeScope,
}

impl MemberBuildSpec {
    pub fn new(id: AgentId) -> Self {
        Self {
            id,
            llm_config: None,
            capabilities: Vec::new(),
            tools: Vec::new(),
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
            tool_policy: None,
            scope: RuntimeScope::Inherit,
        }
    }

    /// The roster entry for this member.
    pub(crate) fn info(&self) -> AgentInfo {
        AgentInfo {
            isolation: self.isolation,
            id: self.id.clone(),
            name: self.id.to_string(),
            description: self.description.clone().unwrap_or_default(),
            capabilities: self.capabilities.clone(),
            required_capabilities: self.required_capabilities.clone(),
            meta: None,
        }
    }

    /// Lowers a `.agents` delegation target. Fails when the profile's model
    /// preset cannot be applied (the plan was diagnosed at plan time).
    pub fn from_protocol_plan(
        plan: &crate::dotagents::DotagentsSubAgentPlan,
        skills: Option<&SkillsConfig>,
    ) -> anyhow::Result<Self> {
        let mut params = LLMParams::new();
        // The profile body is this target's agent-specific system prompt.
        // Frontmatter was already stripped by the parser.
        if !plan.system_prompt.trim().is_empty() {
            params = params.system(plan.system_prompt.clone());
        }
        if let Some(overlay) = &plan.model {
            overlay
                .apply_to(&mut params)
                .map_err(|d| anyhow::anyhow!("model preset of `{}`: {}", plan.id, d.message))?;
        }

        let mut spec = Self::new(plan.info.id.clone());
        spec.llm_config = Some(params);
        spec.description = Some(plan.info.description.clone());
        spec.capabilities = plan.info.capabilities.clone();
        spec.required_capabilities = plan.info.required_capabilities.clone();
        spec.isolation = plan.info.isolation;
        spec.mcp_servers = plan.mcp_servers.clone();
        spec.skills = skills.cloned();
        spec.scope = RuntimeScope::Standalone;
        match plan.tools.as_ref().filter(|tools| !tools.is_empty()) {
            Some(tools) => {
                spec.tools = tools
                    .iter()
                    .filter_map(|tool| match tool.parse() {
                        Ok(tool_spec) => Some(tool_spec),
                        Err(error) => {
                            log::warn!("dotagents: ignoring tool `{tool}`: {error}");
                            None
                        }
                    })
                    .collect();
            }
            None => spec.tool_policy = Some(ToolPolicy::default()),
        }
        Ok(spec)
    }

    /// Lowers a configured delegate. The one conversion used for delegates
    /// from TOML, from the Rust builders and from `querymt/agents/add`.
    ///
    /// `top_level_mcp` are the runtime's own MCP servers, which a tool
    /// selector may name besides the delegate's own `mcp`. `peer` is not part
    /// of the spec: the caller reads it before lowering.
    pub fn from_delegate_config(
        delegate: DelegateConfig,
        top_level_mcp: &[McpServerConfig],
        local_tool_names: &HashSet<String>,
    ) -> anyhow::Result<Self> {
        let mut spec = Self::new(AgentId::try_from(delegate.id.as_str())?);
        let mut llm = LLMParams::new()
            .provider(delegate.provider)
            .model(delegate.model);
        for part in delegate.system {
            if let Some(system) = part.resolved() {
                llm = llm.system(system);
            }
        }
        if let Some(api_key) = delegate.api_key {
            llm = llm.api_key(api_key);
        }
        for (key, value) in delegate.parameters.into_iter().flatten() {
            llm = llm.parameter(key, value);
        }
        spec.llm_config = Some(llm);
        spec.description = delegate.description;
        spec.capabilities = delegate.capabilities;

        let resolved = crate::config::resolve_tools(
            &delegate.tools,
            top_level_mcp,
            &delegate.mcp,
            local_tool_names,
        )?;
        spec.tools = resolved.runtime_tool_selectors();
        spec.mcp_servers = resolved
            .mcp_servers
            .values()
            .map(|(config, _)| config.clone())
            .collect();
        spec.required_capabilities = infer_required_capabilities(&spec.tools)
            .into_iter()
            .collect();
        spec.mode = delegate.mode;
        spec.middleware = delegate.middleware;
        spec.execution = delegate.execution;
        spec.skills = Some(delegate.skills);
        spec.permissions = delegate.permissions.to_permissions();
        spec.isolation = delegate.isolation;
        Ok(spec)
    }

    /// The LLM parameters, or an error when `.provider()` was never called.
    pub fn llm_params(&self) -> anyhow::Result<LLMParams> {
        self.llm_config
            .clone()
            .ok_or_else(|| anyhow::anyhow!("LLM configuration required (call .provider() first)"))
    }

    /// Checks what building this member needs from the runtime it joins.
    pub(crate) fn check_against(&self, ctx: &MemberContext) -> Result<(), RosterError> {
        if self.isolation == DelegationIsolation::Isolated && ctx.params.workspace_service.is_none()
        {
            return Err(RosterError::IsolationWithoutWorkspace {
                id: self.id.clone(),
            });
        }
        Ok(())
    }
}

/// What every delegate of one runtime shares.
#[doc(hidden)]
#[derive(Clone)]
pub struct DelegateBuildParams {
    pub plugin_registry: Arc<PluginRegistry>,
    pub snapshot_policy: SnapshotPolicy,
    pub skills_project_root: PathBuf,
    pub workspace_service: Option<Arc<crate::worktree::WorkspaceService>>,
    pub repository_knowledge:
        Option<Arc<crate::knowledge::repository::RepositoryKnowledgeRegistry>>,
}

impl DelegateBuildParams {
    /// Parameters of a runtime with no snapshots, workspace or knowledge.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(plugin_registry: Arc<PluginRegistry>) -> Self {
        Self {
            plugin_registry,
            snapshot_policy: SnapshotPolicy::None,
            skills_project_root: ".".into(),
            workspace_service: None,
            repository_knowledge: None,
        }
    }
}

/// What every member of one runtime shares: the settings above, the
/// delegation settings, the storage, the event stream and the mesh. Owned by
/// the roster, so a member cannot be built on a different event stream.
pub(crate) struct MemberContext {
    pub params: DelegateBuildParams,
    pub settings: DelegationSettings,
    pub storage: Arc<dyn StorageBackend>,
    pub event_fanout: Arc<EventFanout>,
    /// The mesh every member joins when it is built. Set when the runtime
    /// joins a mesh, so a member built later (on first use, or added at
    /// runtime) joins it too.
    #[cfg(feature = "remote")]
    mesh: parking_lot::Mutex<Option<crate::agent::remote::MeshHandle>>,
}

impl MemberContext {
    pub(crate) fn new(
        params: DelegateBuildParams,
        settings: DelegationSettings,
        storage: Arc<dyn StorageBackend>,
        event_fanout: Arc<EventFanout>,
    ) -> Self {
        Self {
            params,
            settings,
            storage,
            event_fanout,
            #[cfg(feature = "remote")]
            mesh: parking_lot::Mutex::new(None),
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) fn set_mesh(&self, mesh: Option<crate::agent::remote::MeshHandle>) {
        *self.mesh.lock() = mesh;
    }

    #[cfg(feature = "remote")]
    pub(crate) fn mesh(&self) -> Option<crate::agent::remote::MeshHandle> {
        self.mesh.lock().clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn for_tests(
        plugin_registry: Arc<PluginRegistry>,
        storage: Arc<dyn StorageBackend>,
    ) -> Self {
        Self::new(
            DelegateBuildParams::for_tests(plugin_registry),
            DelegationSettings::default(),
            storage,
            Arc::new(EventFanout::new()),
        )
    }
}

/// Builds a local member the way a roster does, for tests outside this crate.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn build_member_for_tests(
    spec: &MemberBuildSpec,
    params: DelegateBuildParams,
    storage: Arc<dyn StorageBackend>,
) -> anyhow::Result<Arc<crate::agent::LocalAgentHandle>> {
    let ctx = MemberContext::new(
        params,
        DelegationSettings::default(),
        storage,
        Arc::new(EventFanout::new()),
    );
    build_local_member(spec, &ctx)
}

/// Builds one local member on the context's event stream. This is the only
/// function that builds a member.
pub(crate) fn build_local_member(
    spec: &MemberBuildSpec,
    ctx: &MemberContext,
) -> anyhow::Result<Arc<crate::agent::LocalAgentHandle>> {
    spec.check_against(ctx)?;
    let params = &ctx.params;
    let tools = &spec.tools;
    let mut b = AgentConfigBuilder::new(
        params.plugin_registry.clone(),
        ctx.storage.clone(),
        spec.llm_params()?,
    )
    .with_agent_id(spec.id.to_string())
    .with_event_fanout(ctx.event_fanout.clone());

    if let Some(skills) = spec.skills.as_ref()
        && skills.enabled
        && (tools.is_empty()
            || tools
                .iter()
                .any(|tool| tool.is_builtin(crate::skills::SkillTool::NAME)))
    {
        let catalog = crate::skills::SkillCatalog::from_config(skills, &params.skills_project_root);
        b = b.with_skill_catalog(catalog);
    }

    // Infer the policy like the single-agent path: any external tool spec,
    // resolved MCP selection or attached MCP server needs BuiltInAndProvider
    // so MCP tools and skills reach the LLM, even when the tool list is
    // empty; otherwise BuiltInOnly is sufficient.
    let policy = spec.tool_policy.unwrap_or_else(|| {
        crate::agent::tools::infer_restricted_tool_policy(tools, &spec.mcp_servers)
    });
    b = b.with_tool_policy(policy);
    if !tools.is_empty() {
        b = b.with_allowed_tools(tools.clone());
    }

    // Attach resolved MCP servers through the existing lifecycle.
    if !spec.mcp_servers.is_empty() {
        b = b.with_mcp_servers(spec.mcp_servers.clone());
    }

    if spec.scope == RuntimeScope::Inherit {
        b = inherit_runtime(b, spec, ctx)?;
    }
    let handle = Arc::new(crate::agent::LocalAgentHandle::from_config(Arc::new(
        b.build(),
    )));
    #[cfg(feature = "remote")]
    if let Some(mesh) = ctx.mesh() {
        handle.set_mesh_local(mesh);
    }
    Ok(handle)
}

/// What a member takes from its runtime: snapshots, delegation settings,
/// mode, permissions, middleware, workspace service and repository knowledge.
fn inherit_runtime(
    mut b: AgentConfigBuilder,
    spec: &MemberBuildSpec,
    ctx: &MemberContext,
) -> anyhow::Result<AgentConfigBuilder> {
    let params = &ctx.params;
    let settings = &ctx.settings;
    b = b.with_snapshot_policy(params.snapshot_policy);
    if params.snapshot_policy != SnapshotPolicy::None {
        b = b.with_snapshot_backend(Arc::new(GitSnapshotBackend::new()));
    }
    b = b
        .with_agent_mode(spec.mode)
        .with_snapshot_from_execution(&spec.execution)
        .with_delegation_wait_policy(settings.wait_policy.clone())
        .with_delegation_wait_timeout_secs(settings.wait_timeout_secs)
        .with_delegation_cancel_grace_secs(settings.cancel_grace_secs)
        .with_permissions(spec.permissions.clone());
    apply_middleware_from_config(&mut b, &spec.middleware)?;
    if let Some(service) = params.workspace_service.clone() {
        b = b.with_workspace_service(service);
    }
    Ok(b.with_repository_knowledge(params.repository_knowledge.clone()))
}

/// Applies middleware from config entries (plus the shared defaults) to a builder.
///
/// Uses the same assembly path as `AgentBuilder::build`, so an invalid entry
/// fails identically for single agents, planners and delegates.
///
/// # Errors
///
/// Fails when an entry has an unknown type or invalid configuration.
fn apply_middleware_from_config(
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
    .with_execution_policy(ephemeral_policy)
    .with_tool_registry(builder.tool_registry_snapshot())
    .build();

    let existing = builder.middleware_snapshot();
    for driver in crate::middleware::build_from_entries(entries, &existing, &factory_config)? {
        builder.push_middleware(driver);
    }
    Ok(())
}
