use super::*;

/// Multi-agent quorum configuration (planner + delegates).
///
/// A quorum consists of a planner agent that decomposes tasks and one or more
/// delegate agents that execute them. The planner uses the `delegate` built-in
/// tool to assign work.
///
/// ```toml
/// [quorum]
/// cwd = "."
/// delegation = true
///
/// [planner]
/// provider = "anthropic"
/// model = "claude-sonnet-4-5-20250929"
/// tools = ["delegate", "read_tool", "shell"]
///
/// [[delegates]]
/// id = "coder"
/// provider = "anthropic"
/// model = "claude-sonnet-4-5-20250929"
/// tools = ["edit", "write_file", "shell", "read_tool", "glob"]
/// ```
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuorumConfig {
    /// Top-level quorum orchestration settings.
    pub quorum: QuorumSettings,

    /// MCP servers available to all agents in the quorum.
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,

    /// The planner agent that decomposes tasks and delegates to workers.
    /// Should have the `"delegate"` tool enabled.
    pub planner: PlannerConfig,

    /// Delegate (worker) agents that execute tasks assigned by the planner.
    #[serde(default)]
    pub delegates: Vec<DelegateConfig>,

    /// Optional kameo libp2p mesh for cross-machine delegation.
    #[serde(default)]
    pub mesh: MeshTomlConfig,

    /// Remote agents on other mesh nodes available for delegation.
    /// Requires `[mesh] enabled = true`.
    #[serde(default, rename = "remote_agents")]
    pub remote_agents: Vec<RemoteAgentConfig>,

    /// Optional `.agents` Protocol loading settings (disabled by default).
    #[serde(default)]
    pub dotagents: crate::dotagents::DotagentsSettings,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DelegationWaitPolicy {
    All,
    #[default]
    Any,
}

fn default_delegation_wait_timeout_secs() -> u64 {
    120
}

fn default_delegation_cancel_grace_secs() -> u64 {
    5
}

fn default_max_parallel_delegations() -> usize {
    5
}

/// Top-level settings for a multi-agent quorum (planner + delegates).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuorumSettings {
    /// Working directory for the quorum. Relative paths are resolved against this.
    pub cwd: Option<PathBuf>,

    /// Enable task delegation from the planner to delegate agents. Default: `true`.
    #[serde(default = "default_true")]
    pub delegation: bool,

    /// Enable verification pass after delegation completes. Default: `false`.
    #[serde(default)]
    pub verification: bool,

    /// Snapshot policy for capturing file state during delegation.
    /// - `"diff"`: Capture file diffs between snapshots.
    /// - `"metadata"`: Capture file metadata only.
    /// - absent / `null`: No snapshots.
    #[serde(default)]
    #[schemars(extend("enum" = ["diff", "metadata"]))]
    pub snapshot_policy: Option<String>,

    /// Optional LLM call that summarises the planner conversation before
    /// handing off to a delegate, providing richer context.
    pub delegation_summary: Option<DelegationSummaryConfig>,

    /// How to handle multiple concurrent delegations completing.
    /// - `"any"` (default): proceed when the first delegate finishes.
    /// - `"all"`: wait for all delegates to finish.
    #[serde(default)]
    pub delegation_wait_policy: DelegationWaitPolicy,

    /// Timeout in seconds to wait for delegations to complete. Default: `120`.
    #[serde(default = "default_delegation_wait_timeout_secs")]
    pub delegation_wait_timeout_secs: u64,

    /// Grace period in seconds to allow in-flight delegates to finish after
    /// the wait policy threshold is met. Default: `5`.
    #[serde(default = "default_delegation_cancel_grace_secs")]
    pub delegation_cancel_grace_secs: u64,

    /// Maximum number of delegates that can run in parallel. Default: `5`.
    #[serde(default = "default_max_parallel_delegations")]
    pub max_parallel_delegations: usize,

    /// Managed Git worktree workspaces (`[quorum.workspace]`). One service is
    /// shared by the planner and every delegate. Disabled by default.
    #[serde(default)]
    pub workspace: WorkspaceSettings,

    /// Portable repository knowledge (`[quorum.repository_knowledge]`). One
    /// service is shared across the quorum. Disabled by default.
    #[serde(default)]
    pub repository_knowledge: RepositoryKnowledgeSettings,
}

fn default_true() -> bool {
    true
}

/// Planner agent configuration.
///
/// The planner decomposes tasks and delegates subtasks to worker agents.
/// It should have the `"delegate"` built-in tool enabled.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlannerConfig {
    /// LLM provider name (e.g. `"anthropic"`, `"openai"`).
    pub provider: String,

    /// Model identifier for the planner (typically a capable reasoning model).
    pub model: String,

    /// API key override. Supports `${VAR}` interpolation.
    pub api_key: Option<String>,

    /// Tools available to the planner. The `"delegate"` tool is required for
    /// task delegation. Typical set: `["delegate", "read_tool", "shell", "glob"]`.
    #[serde(default)]
    #[schemars(extend("examples" = [
        ["delegate", "read_tool", "shell", "glob"],
        ["delegate", "read_tool", "shell", "glob", "search_text", "question"]
    ]))]
    pub tools: Vec<ToolSpec>,

    /// Initial mode of new planner sessions: `build` (default), `plan` or `review`.
    #[serde(default)]
    pub mode: crate::agent::core::AgentMode,

    /// System prompt for the planner. Accepts a string or mixed array of
    /// inline strings and `{ file = "path" }` references.
    #[serde(default, deserialize_with = "deserialize_system_parts")]
    #[schemars(schema_with = "crate::config::schema_for_system_parts")]
    pub system: Vec<SystemPart>,

    /// Extra parameters forwarded to the LLM provider API.
    #[serde(default)]
    pub parameters: Option<HashMap<String, Value>>,

    /// Middleware stack for the planner (limits, context management, etc.).
    #[serde(default)]
    pub middleware: Vec<MiddlewareEntry>,

    /// Execution policy (tool output, pruning, compaction, snapshot, rate limit).
    #[serde(default)]
    pub execution: ExecutionPolicy,

    /// Skills system configuration.
    #[serde(default)]
    pub skills: SkillsConfig,

    /// Tool approval for the planner.
    #[serde(default)]
    pub permissions: PermissionsConfig,
}
