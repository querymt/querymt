use super::*;

/// Single agent configuration.
///
/// Configures a single AI agent with tools, MCP servers, and middleware.
/// The `[agent]` section is required; all other sections are optional.
///
/// ```toml
/// [agent]
/// provider = "anthropic"
/// model = "claude-sonnet-4-5-20250929"
/// tools = ["read_tool", "edit", "write_file", "shell", "glob", "search_text"]
/// system = "You are a helpful coding assistant."
/// ```
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{
    "agent": {
        "provider": "anthropic",
        "model": "claude-sonnet-4-5-20250929",
        "tools": ["read_tool", "edit", "write_file", "shell", "glob", "search_text"],
        "system": "You are a helpful coding assistant.",
        "mutating_tools": ["edit", "write_file", "shell"],
        "assume_mutating": false,
        "execution": {
            "snapshot": {"backend": "git"},
            "compaction": {"auto": true}
        }
    },
    "mcp": [
        {"name": "filesystem", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]}
    ],
    "middleware": [
        {"type": "limits", "max_steps": 200, "max_turns": 50},
        {"type": "context", "warn_at_percent": 80, "compact_at_percent": 90}
    ]
}]))]
pub struct SingleAgentConfig {
    /// Core agent settings: provider, model, tools, system prompt.
    pub agent: AgentSettings,

    /// MCP (Model Context Protocol) servers that provide additional tools.
    /// Use `transport = "stdio"` for local command-based servers,
    /// `transport = "http"` for remote HTTP servers.
    /// NOTE: this is NOT for remote agents — use `[[remote_agents]]` with `[mesh]` for that.
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,

    /// Middleware stack applied in order. Controls execution limits, context
    /// management, deduplication checks, and agent mode switching.
    #[serde(default)]
    pub middleware: Vec<MiddlewareEntry>,

    /// Optional kameo libp2p mesh for cross-machine collaboration.
    /// Required when using `[[remote_agents]]`.
    #[serde(default)]
    pub mesh: MeshTomlConfig,

    /// Remote agents running on OTHER mesh nodes, used for delegation.
    /// Requires `[mesh] enabled = true`.
    /// NOTE: this is NOT for MCP servers — use `[[mcp]]` for those.
    #[serde(default, rename = "remote_agents")]
    pub remote_agents: Vec<RemoteAgentConfig>,
}

/// A middleware entry in the agent's processing stack.
///
/// The `type` field selects which middleware to use; all other fields are
/// forwarded to that middleware as configuration.
///
/// Available middleware types:
/// - `"limits"` — hard cap on steps and turns (`max_steps`, `max_turns`)
/// - `"context"` — token-window management (`warn_at_percent`, `compact_at_percent`, `fallback_max_tokens`)
/// - `"dedup_check"` — detects repeated/similar code output (`threshold`, `min_lines`)
/// - `"agent_mode"` — build/plan/review mode switching (`default`, `reminder`, `review_reminder`)
/// - `"plan_mode"` — legacy read-only mode (prefer `agent_mode`)
///
/// ```toml
/// [[middleware]]
/// type = "limits"
/// max_steps = 200
/// max_turns = 50
///
/// [[middleware]]
/// type = "context"
/// warn_at_percent = 80
/// compact_at_percent = 90
/// ```
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[schemars(extend("examples" = [
    {"type": "limits", "max_steps": 200, "max_turns": 50},
    {"type": "context", "warn_at_percent": 80, "compact_at_percent": 90, "fallback_max_tokens": 128000},
    {"type": "dedup_check", "threshold": 0.85, "min_lines": 10},
    {"type": "agent_mode", "default": "build"}
]))]
pub struct MiddlewareEntry {
    /// The middleware type name. Built-in types: `"limits"`, `"context"`,
    /// `"dedup_check"`, `"agent_mode"`, `"plan_mode"`.
    /// Custom middleware types can be registered at runtime.
    #[serde(rename = "type")]
    #[schemars(extend("enum" = ["limits", "context", "dedup_check", "agent_mode", "plan_mode"]))]
    pub middleware_type: String,

    /// Middleware-specific configuration fields.
    /// Valid keys depend on the `type` — see middleware documentation.
    #[serde(flatten)]
    #[schemars(schema_with = "crate::config::schema_for_value")]
    pub config: serde_json::Value,
}

/// Agent settings for single agent mode
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    /// Working directory for the agent. Relative paths in tool calls are resolved
    /// against this directory. Defaults to the process working directory.
    pub cwd: Option<PathBuf>,

    /// LLM provider name. Providers are loaded from `~/.qmt/providers.toml`.
    /// Common values: `"anthropic"`, `"openai"`, `"ollama"`, `"llama_cpp"`.
    pub provider: String,

    /// Model identifier passed to the provider.
    /// Examples: `"claude-sonnet-4-5-20250929"` (Anthropic), `"gpt-4o"` (OpenAI),
    /// `"llama3.1:latest"` (Ollama), `"qwen3-coder-30b"` (llama_cpp).
    pub model: String,

    /// API key for the provider. Supports environment variable interpolation:
    /// `"${ANTHROPIC_API_KEY}"` or `"${KEY:-fallback}"`.
    /// Optional for local providers (ollama, llama_cpp).
    pub api_key: Option<String>,

    /// Built-in tool names to enable, or MCP tool patterns.
    /// When empty, all built-in tools are available.
    ///
    /// Built-in tools: `read_tool`, `index`, `edit`, `write_file`, `delete_file`,
    /// `shell`, `glob`, `search_text`, `ls`, `web_fetch`, `browse`, `mdq`,
    /// `question`, `delegate`, `create_task`, `todowrite`, `todoread`,
    /// `semantic_edit`, `multiedit`, `get_function`, `get_symbol`,
    /// `replace_symbol`, `find_symbol_references`,
    /// `knowledge_consolidate`, `knowledge_ingest`, `knowledge_list_unconsolidated`,
    /// `knowledge_query`, `knowledge_stats`, `language_query`.
    ///
    /// MCP patterns: `"server_name.*"` (all tools from server),
    /// `"server_name.tool_name"` (specific tool).
    #[serde(default)]
    #[schemars(extend("examples" = [
        ["read_tool", "index", "edit", "write_file", "shell", "glob", "search_text"],
        ["read_tool", "index", "edit", "write_file", "shell", "glob", "search_text", "ls", "web_fetch", "question", "create_task", "todowrite", "todoread"],
        ["read_tool", "index", "glob", "search_text", "filesystem.*"]
    ]))]
    pub tools: Vec<String>,

    /// System prompt. Accepts a plain string, an array of strings, or a mixed
    /// array of inline strings and `{ file = "path" }` file references.
    #[serde(default, deserialize_with = "deserialize_system_parts")]
    #[schemars(schema_with = "crate::config::schema_for_system_parts")]
    pub system: Vec<SystemPart>,

    /// Extra parameters forwarded verbatim to the LLM provider API
    /// (e.g. `temperature`, `max_tokens`, `top_p`).
    #[serde(default)]
    pub parameters: Option<HashMap<String, Value>>,

    /// Whether to treat unknown tools (e.g. MCP tools not listed in
    /// `mutating_tools`) as mutating, requiring permission confirmation.
    /// Set to `false` and use `mutating_tools` for explicit control.
    #[serde(default = "crate::config::default_assume_mutating")]
    pub assume_mutating: bool,

    /// Explicit allowlist of tools that modify the filesystem or execute
    /// commands and therefore require permission confirmation.
    /// Common values: `["edit", "write_file", "delete_file", "shell"]`.
    #[serde(default)]
    #[schemars(extend("examples" = [["edit", "write_file", "shell"], ["edit", "write_file", "delete_file", "shell"]]))]
    pub mutating_tools: Vec<String>,

    /// Execution policy (tool output truncation, pruning, compaction, snapshot, rate limit).
    #[serde(default)]
    pub execution: ExecutionPolicy,

    /// Skills system configuration.
    #[serde(default)]
    pub skills: SkillsConfig,

    /// Slash commands system configuration.
    #[serde(default)]
    pub slash_commands: SlashCommandsConfig,
}

#[allow(dead_code)]
fn default_assume_mutating() -> bool {
    true
}
