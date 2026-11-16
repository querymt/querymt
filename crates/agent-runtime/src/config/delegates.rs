//! Settings of a runtime's delegation service.

use super::*;

/// How the delegation service of one runtime behaves.
///
/// The defaults are those of the `[quorum]` keys these settings replace.
///
/// ```toml
/// [agent.delegation]
/// wait_policy = "all"
/// wait_timeout_secs = 300
/// max_parallel = 2
/// ```
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DelegationSettings {
    /// How to handle several delegations completing.
    pub wait_policy: DelegationWaitPolicy,
    /// Seconds to wait for delegations to complete.
    pub wait_timeout_secs: u64,
    /// Seconds in-flight delegates may keep running after the wait policy is met.
    pub cancel_grace_secs: u64,
    /// Delegations that may run in parallel. Must be at least 1.
    pub max_parallel: std::num::NonZeroUsize,
    /// Run the verification pass after a delegation completes.
    pub verification: bool,
    /// Optional summary of the parent conversation handed to the delegate.
    pub summary: Option<DelegationSummaryConfig>,
}

impl Default for DelegationSettings {
    fn default() -> Self {
        Self {
            wait_policy: DelegationWaitPolicy::default(),
            wait_timeout_secs: 120,
            cancel_grace_secs: 5,
            max_parallel: std::num::NonZeroUsize::new(5).expect("non-zero default"),
            verification: false,
            summary: None,
        }
    }
}

/// Delegate (worker) agent configuration.
///
/// Delegates execute tasks assigned by the planner. Each delegate has its own
/// tool set, system prompt, and optionally its own MCP servers.
#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DelegateConfig {
    /// Unique identifier for this delegate, used by the planner to target it.
    pub id: String,

    /// LLM provider name (e.g. `"anthropic"`, `"openai"`, `"llama_cpp"`).
    pub provider: String,

    /// Model identifier for this delegate.
    pub model: String,

    /// API key override. Supports `${VAR}` interpolation.
    pub api_key: Option<String>,

    /// Human-readable description shown to the planner when choosing a delegate.
    pub description: Option<String>,

    /// Capability tags used by the planner to select suitable delegates
    /// (e.g. `["coding", "filesystem", "gpu"]`).
    #[serde(default)]
    pub capabilities: Vec<String>,

    /// Tools available to this delegate. Typical coder set:
    /// `["edit", "write_file", "shell", "read_tool", "glob", "search_text"]`.
    #[serde(default)]
    #[schemars(extend("examples" = [
        ["edit", "write_file", "shell", "read_tool", "glob", "search_text"],
        ["edit", "write_file", "read_tool", "glob"]
    ]))]
    pub tools: Vec<ToolSpec>,

    /// Initial mode of new delegate sessions: `build` (default), `plan` or `review`.
    #[serde(default)]
    pub mode: crate::agent::core::AgentMode,

    /// System prompt for this delegate. Accepts a string or mixed array of
    /// inline strings and `{ file = "path" }` references.
    #[serde(default, deserialize_with = "deserialize_system_parts")]
    #[schemars(schema_with = "crate::config::schema_for_system_parts")]
    pub system: Vec<SystemPart>,

    /// Extra parameters forwarded to the LLM provider API.
    #[serde(default)]
    pub parameters: Option<HashMap<String, Value>>,

    /// MCP servers specific to this delegate (in addition to quorum-level MCP).
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,

    /// Middleware stack for this delegate (limits, context management, etc.).
    #[serde(default)]
    pub middleware: Vec<MiddlewareEntry>,

    /// Execution policy (tool output, pruning, compaction, snapshot, rate limit).
    #[serde(default)]
    pub execution: ExecutionPolicy,

    /// Skills system configuration.
    #[serde(default)]
    pub skills: SkillsConfig,

    /// Tool approval for this delegate.
    #[serde(default)]
    pub permissions: PermissionsConfig,

    /// Optional mesh peer name (references a `[[mesh.peers]]` entry).
    ///
    /// When set, LLM calls for this delegate are routed to the remote peer via
    /// the mesh while tool execution continues locally on the planner node.
    /// This is the "remote model, local session" pattern.
    ///
    /// Requires `[mesh] enabled = true`. Validated at startup.
    #[serde(default)]
    pub peer: Option<String>,

    /// Workspace isolation for tasks delegated to this delegate.
    ///
    /// Precedence: an explicit non-`inherit` value on the `delegate` tool call,
    /// then this value, then the parent's execution mode. `isolated` requires
    /// `[quorum.workspace] enabled = true`.
    #[serde(default)]
    pub isolation: crate::worktree::DelegationIsolation,
}

/// `Debug` that never prints `api_key`: a `querymt/agents/add` request holds
/// a client-supplied secret, and a debug log of it must not leak it.
impl std::fmt::Debug for DelegateConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegateConfig")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("description", &self.description)
            .field("capabilities", &self.capabilities)
            .field("tools", &self.tools)
            .field("mode", &self.mode)
            .field("system", &self.system)
            .field("parameters", &self.parameters)
            .field("mcp", &self.mcp)
            .field("middleware", &self.middleware)
            .field("execution", &self.execution)
            .field("skills", &self.skills)
            .field("permissions", &self.permissions)
            .field("peer", &self.peer)
            .field("isolation", &self.isolation)
            .finish()
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn debug_output_redacts_the_api_key() {
        let delegate: DelegateConfig = serde_json::from_value(serde_json::json!({
            "id": "coder", "provider": "p", "model": "m", "api_key": "sk-secret-value"
        }))
        .unwrap();
        let printed = format!("{delegate:?}");
        assert!(!printed.contains("sk-secret-value"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }
}
