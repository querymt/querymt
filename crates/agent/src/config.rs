//! Configuration file support for agents
//!
//! Supports both single-agent and multi-agent (quorum) configurations from TOML files.

use agent_client_protocol::{
    EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
};
use anyhow::{Context, Result, anyhow};
use regex::{Captures, Regex};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ============================================================================
// Compaction Configuration (3-Layer System)
// ============================================================================

/// Default maximum lines before truncation
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Default maximum bytes before truncation (50 KB)
pub const DEFAULT_MAX_BYTES: usize = 51200;

/// Default tokens to protect from pruning
pub const DEFAULT_PRUNE_PROTECT_TOKENS: usize = 40_000;

/// Default minimum tokens required before pruning
pub const DEFAULT_PRUNE_MINIMUM_TOKENS: usize = 20_000;

/// Default protected tools that should never be pruned
pub const DEFAULT_PROTECTED_TOOLS: &[&str] = &["skill"];

/// Default maximum retry attempts for compaction
pub const DEFAULT_MAX_RETRIES: usize = 3;

/// Default initial backoff in milliseconds
pub const DEFAULT_INITIAL_BACKOFF_MS: u64 = 1000;

/// Default backoff multiplier
pub const DEFAULT_BACKOFF_MULTIPLIER: f64 = 2.0;

fn default_max_lines() -> usize {
    DEFAULT_MAX_LINES
}

fn default_max_bytes() -> usize {
    DEFAULT_MAX_BYTES
}

fn default_prune_protect_tokens() -> usize {
    DEFAULT_PRUNE_PROTECT_TOKENS
}

fn default_prune_minimum_tokens() -> usize {
    DEFAULT_PRUNE_MINIMUM_TOKENS
}

fn default_protected_tools() -> Vec<String> {
    DEFAULT_PROTECTED_TOOLS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_max_retries() -> usize {
    DEFAULT_MAX_RETRIES
}

fn default_initial_backoff_ms() -> u64 {
    DEFAULT_INITIAL_BACKOFF_MS
}

fn default_backoff_multiplier() -> f64 {
    DEFAULT_BACKOFF_MULTIPLIER
}

/// Where to store overflow output when tool output is truncated
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverflowStorage {
    /// Discard overflow (don't save)
    Discard,
    /// Save to temp directory (/tmp/qmt-tool-outputs/{session_id}/)
    #[default]
    TempDir,
    /// Save to persistent data directory
    DataDir,
    // TODO: Database storage option for future implementation
}

/// Configuration for tool output truncation (Layer 1)
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputConfig {
    /// Maximum lines before truncation
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,

    /// Maximum bytes before truncation
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,

    /// Where to save full output when truncated
    #[serde(default)]
    pub overflow_storage: OverflowStorage,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            overflow_storage: OverflowStorage::default(),
        }
    }
}

/// Configuration for pruning (Layer 2) - runs after every turn
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PruningConfig {
    /// Enable/disable pruning
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Tokens of recent tool outputs to protect from pruning
    #[serde(default = "default_prune_protect_tokens")]
    pub protect_tokens: usize,

    /// Minimum tokens to clear before pruning (avoids small pruning operations)
    #[serde(default = "default_prune_minimum_tokens")]
    pub minimum_tokens: usize,

    /// Tools that should never be pruned
    #[serde(default = "default_protected_tools")]
    pub protected_tools: Vec<String>,
}

impl Default for PruningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            protect_tokens: DEFAULT_PRUNE_PROTECT_TOKENS,
            minimum_tokens: DEFAULT_PRUNE_MINIMUM_TOKENS,
            protected_tools: default_protected_tools(),
        }
    }
}

/// Retry configuration for compaction LLM calls
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryConfig {
    /// Maximum retry attempts
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,

    /// Initial backoff delay in milliseconds
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,

    /// Exponential backoff multiplier
    #[serde(default = "default_backoff_multiplier")]
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff_ms: DEFAULT_INITIAL_BACKOFF_MS,
            backoff_multiplier: DEFAULT_BACKOFF_MULTIPLIER,
        }
    }
}

/// Configuration for AI compaction (Layer 3) - runs on context overflow
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    /// Enable/disable AI compaction (setting true auto-enables ContextMiddleware)
    #[serde(default = "default_true")]
    pub auto: bool,

    /// Optional: different provider for compaction (cheaper model)
    pub provider: Option<String>,

    /// Optional: different model for compaction (cheaper model)
    pub model: Option<String>,

    /// Retry configuration for compaction LLM calls
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: true,
            provider: None,
            model: None,
            retry: RetryConfig::default(),
        }
    }
}

// ============================================================================
// End Compaction Configuration
// ============================================================================

// ============================================================================
// Snapshot Backend Configuration
// ============================================================================

fn default_snapshot_backend() -> String {
    "none".to_string()
}

fn default_max_snapshots() -> Option<usize> {
    Some(100)
}

fn default_max_age_days() -> Option<u64> {
    Some(30)
}

/// Configuration for snapshot backend (undo/redo support)
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotBackendConfig {
    /// Backend type: "git" or "none" (default: "none")
    #[serde(default = "default_snapshot_backend")]
    pub backend: String,

    /// Maximum number of snapshots to keep (oldest are removed first)
    #[serde(default = "default_max_snapshots")]
    pub max_snapshots: Option<usize>,

    /// Maximum age of snapshots in days (older are removed)
    #[serde(default = "default_max_age_days")]
    pub max_age_days: Option<u64>,
}

impl Default for SnapshotBackendConfig {
    fn default() -> Self {
        Self {
            backend: default_snapshot_backend(),
            max_snapshots: default_max_snapshots(),
            max_age_days: default_max_age_days(),
        }
    }
}

// ============================================================================
// End Snapshot Backend Configuration
// ============================================================================

/// A single part of a system prompt, either an inline string or a file reference.
///
/// In TOML configs, the `system` field accepts a mixed array of strings and
/// `{ file = "path" }` objects, preserving order:
///
/// ```toml
/// system = [
///   "You are a helpful assistant.",
///   { file = "prompts/coder.md" },
///   "Additional instructions.",
/// ]
/// ```
///
/// For convenience, a plain string is also accepted:
///
/// ```toml
/// system = "You are a helpful assistant."
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SystemPart {
    /// An inline system prompt string
    Inline(String),
    /// A file reference whose contents will be loaded as a system prompt part
    File { file: PathBuf },
}

/// Deserializes the `system` field which can be:
/// - absent → empty vec
/// - a single string → `[Inline(s)]`
/// - an array of mixed strings and `{ file = "..." }` objects → `Vec<SystemPart>`
fn deserialize_system_parts<'de, D>(deserializer: D) -> Result<Vec<SystemPart>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SystemField {
        Single(String),
        Multiple(Vec<SystemPart>),
    }
    match Option::<SystemField>::deserialize(deserializer)? {
        None => Ok(Vec::new()),
        Some(SystemField::Single(s)) => Ok(vec![SystemPart::Inline(s)]),
        Some(SystemField::Multiple(v)) => Ok(v),
    }
}

/// Resolves a list of system parts into a flat list of strings by reading file contents.
async fn resolve_system_parts(
    parts: &[SystemPart],
    base_path: &Path,
    context: &str,
) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            SystemPart::Inline(s) => resolved.push(s.clone()),
            SystemPart::File { file } => {
                let path = base_path.join(file);
                let content = tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("Failed to load {context} prompt from {path:?}"))?;
                let content = interpolate_env_vars(&content).with_context(|| {
                    format!("Failed to interpolate env vars in {context} prompt from {path:?}")
                })?;
                resolved.push(content);
            }
        }
    }
    Ok(resolved)
}

/// Top-level config discriminator
#[derive(Debug)]
pub enum Config {
    Single(SingleAgentConfig),
    Multi(QuorumConfig),
}

/// Single agent configuration
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SingleAgentConfig {
    pub agent: AgentSettings,
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,
    #[serde(default)]
    pub middleware: Vec<MiddlewareEntry>,
}

/// Raw middleware entry from TOML config
///
/// The `type` field determines which middleware factory to use.
/// All other fields are passed to the factory as a JSON value.
///
/// # Example
///
/// ```toml
/// [[middleware]]
/// type = "dedup_check"
/// threshold = 0.8
/// min_lines = 5
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct MiddlewareEntry {
    /// The middleware type name (e.g., "dedup_check")
    #[serde(rename = "type")]
    pub middleware_type: String,
    /// All other config fields, passed to the middleware factory
    #[serde(flatten)]
    pub config: serde_json::Value,
}

/// Agent settings for single agent mode
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSettings {
    pub cwd: Option<PathBuf>,
    pub db: Option<PathBuf>,
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_system_parts")]
    pub system: Vec<SystemPart>,
    #[serde(default)]
    pub parameters: Option<HashMap<String, Value>>,
    /// Tool output truncation settings (Layer 1)
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
    /// Pruning settings - runs after every turn (Layer 2)
    #[serde(default)]
    pub pruning: PruningConfig,
    /// AI compaction settings - runs on context overflow (Layer 3)
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Snapshot backend for undo/redo support
    #[serde(default)]
    pub snapshot: SnapshotBackendConfig,
}

/// Multi-agent quorum configuration
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuorumConfig {
    pub quorum: QuorumSettings,
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,
    pub planner: PlannerConfig,
    #[serde(default)]
    pub delegates: Vec<DelegateConfig>,
}

/// Quorum-level settings
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuorumSettings {
    pub cwd: Option<PathBuf>,
    pub db: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub delegation: bool,
    #[serde(default)]
    pub verification: bool,
    #[serde(default = "default_snapshot_policy")]
    pub snapshot_policy: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_snapshot_policy() -> Option<String> {
    None
}

/// Planner agent configuration
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerConfig {
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_system_parts")]
    pub system: Vec<SystemPart>,
    #[serde(default)]
    pub parameters: Option<HashMap<String, Value>>,
    #[serde(default)]
    pub middleware: Vec<MiddlewareEntry>,
}

/// Delegate agent configuration
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateConfig {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub api_key: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_system_parts")]
    pub system: Vec<SystemPart>,
    #[serde(default)]
    pub parameters: Option<HashMap<String, Value>>,
    #[serde(default)]
    pub mcp: Vec<McpServerConfig>,
    #[serde(default)]
    pub middleware: Vec<MiddlewareEntry>,
}

/// MCP server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum McpServerConfig {
    #[serde(rename_all = "snake_case")]
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    #[serde(rename_all = "snake_case")]
    Http {
        name: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    #[serde(rename_all = "snake_case")]
    Sse {
        name: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl McpServerConfig {
    /// Get the name of the MCP server
    pub fn name(&self) -> &str {
        match self {
            McpServerConfig::Stdio { name, .. } => name,
            McpServerConfig::Http { name, .. } => name,
            McpServerConfig::Sse { name, .. } => name,
        }
    }

    /// Convert to agent-client-protocol McpServer type
    pub fn to_acp(&self) -> McpServer {
        match self {
            McpServerConfig::Stdio {
                name,
                command,
                args,
                env,
            } => {
                let server = McpServerStdio::new(name.clone(), PathBuf::from(command))
                    .args(args.clone())
                    .env(
                        env.iter()
                            .map(|(k, v)| EnvVariable::new(k.clone(), v.clone()))
                            .collect(),
                    );
                McpServer::Stdio(server)
            }
            McpServerConfig::Http { name, url, headers } => {
                let server = McpServerHttp::new(name.clone(), url.clone()).headers(
                    headers
                        .iter()
                        .map(|(k, v)| HttpHeader::new(k.clone(), v.clone()))
                        .collect(),
                );
                McpServer::Http(server)
            }
            McpServerConfig::Sse { name, url, headers } => {
                let server = McpServerSse::new(name.clone(), url.clone()).headers(
                    headers
                        .iter()
                        .map(|(k, v)| HttpHeader::new(k.clone(), v.clone()))
                        .collect(),
                );
                McpServer::Sse(server)
            }
        }
    }

    /// Convert from agent-client-protocol McpServer type
    pub fn from_acp(server: &McpServer) -> Self {
        match server {
            McpServer::Stdio(s) => McpServerConfig::Stdio {
                name: s.name.clone(),
                command: s.command.to_string_lossy().into_owned(),
                args: s.args.clone(),
                env: s
                    .env
                    .iter()
                    .map(|e| (e.name.clone(), e.value.clone()))
                    .collect(),
            },
            McpServer::Http(s) => McpServerConfig::Http {
                name: s.name.clone(),
                url: s.url.clone(),
                headers: s
                    .headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect(),
            },
            McpServer::Sse(s) => McpServerConfig::Sse {
                name: s.name.clone(),
                url: s.url.clone(),
                headers: s
                    .headers
                    .iter()
                    .map(|h| (h.name.clone(), h.value.clone()))
                    .collect(),
            },
            // McpServer is non-exhaustive, handle unknown variants
            _ => panic!("Unknown MCP server transport type"),
        }
    }
}

/// Tool specification parsed from string
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    Builtin(String),             // "edit"
    McpAll(String),              // "github.*"
    McpSpecific(String, String), // "github.search_repos"
}

/// Parse a tool specification string
pub fn parse_tool_spec(tool: &str) -> ToolSpec {
    if let Some(mcp_name) = tool.strip_suffix(".*") {
        ToolSpec::McpAll(mcp_name.to_string())
    } else if let Some((mcp_name, tool_name)) = tool.split_once('.') {
        ToolSpec::McpSpecific(mcp_name.to_string(), tool_name.to_string())
    } else {
        ToolSpec::Builtin(tool.to_string())
    }
}

/// Resolved tools for an agent
#[derive(Debug, Clone)]
pub struct ResolvedTools {
    pub builtins: Vec<String>,
    pub mcp_servers: HashMap<String, (McpServerConfig, Option<Vec<String>>)>,
}

/// Recursively interpolate environment variables in a TOML value
/// Only interpolates strings; leaves comments untouched (they're stripped during parsing)
fn interpolate_toml_value(value: &mut toml::Value) -> Result<()> {
    match value {
        toml::Value::String(s) => {
            *s = interpolate_env_vars(s)?;
        }
        toml::Value::Array(arr) => {
            for item in arr {
                interpolate_toml_value(item)?;
            }
        }
        toml::Value::Table(table) => {
            for (_key, val) in table {
                interpolate_toml_value(val)?;
            }
        }
        // Other types (Integer, Float, Boolean, Datetime) don't contain env vars
        _ => {}
    }
    Ok(())
}

/// Load and parse a config file
pub async fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read config file: {:?}", path))?;

    // Step 1: Parse TOML to strip comments and get structured data
    let mut value: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse TOML config file: {:?}", path))?;

    // Step 2: Interpolate environment variables only in string values
    interpolate_toml_value(&mut value)?;

    // Step 3: Detect config type and deserialize
    let base_path = path.parent().unwrap_or(Path::new("."));

    let config = if value.get("agent").is_some() {
        // Single agent config
        let mut config: SingleAgentConfig = value
            .try_into()
            .with_context(|| "Failed to deserialize single agent config")?;

        // Step 4: Validate
        validate_mcp_servers(&config.mcp)?;

        // Step 5: Resolve system prompt file references
        let resolved = resolve_system_parts(&config.agent.system, base_path, "agent").await?;
        config.agent.system = resolved.into_iter().map(SystemPart::Inline).collect();

        Config::Single(config)
    } else if value.get("quorum").is_some() || value.get("planner").is_some() {
        // Multi-agent config
        let mut config: QuorumConfig = value
            .try_into()
            .with_context(|| "Failed to deserialize quorum config")?;

        // Step 4: Validate
        validate_mcp_servers(&config.mcp)?;
        for delegate in &config.delegates {
            validate_mcp_servers(&delegate.mcp)?;
        }

        // Step 5: Resolve system prompt file references
        let resolved = resolve_system_parts(&config.planner.system, base_path, "planner").await?;
        config.planner.system = resolved.into_iter().map(SystemPart::Inline).collect();
        for delegate in &mut config.delegates {
            let context = format!("delegate '{}'", delegate.id);
            let resolved = resolve_system_parts(&delegate.system, base_path, &context).await?;
            delegate.system = resolved.into_iter().map(SystemPart::Inline).collect();
        }

        Config::Multi(config)
    } else {
        return Err(anyhow!(
            "Invalid config file: must contain [agent] for single agent or [quorum]/[planner] for multi-agent"
        ));
    };

    Ok(config)
}

/// Interpolate environment variables in config content
/// Supports ${VAR} and ${VAR:-default} syntax
pub fn interpolate_env_vars(content: &str) -> Result<String> {
    let re = Regex::new(r"\$\{([A-Z_][A-Z0-9_]*)(?::-([^}]*))?\}")
        .context("Failed to compile env var regex")?;

    let mut errors = Vec::new();

    let result = re.replace_all(content, |caps: &Captures| {
        let var_name = &caps[1];
        let default = caps.get(2).map(|m| m.as_str());

        match (std::env::var(var_name), default) {
            (Ok(val), _) => val,
            (Err(_), Some(default)) => default.to_string(),
            (Err(_), None) => {
                errors.push(var_name.to_string());
                String::new() // Placeholder, will error below
            }
        }
    });

    if !errors.is_empty() {
        return Err(anyhow!(
            "Required environment variables not set: {}",
            errors.join(", ")
        ));
    }

    Ok(result.into_owned())
}

/// Validate MCP servers have unique names
fn validate_mcp_servers(servers: &[McpServerConfig]) -> Result<()> {
    let mut seen = HashSet::new();
    for server in servers {
        let name = server.name();
        if !seen.insert(name) {
            return Err(anyhow!("Duplicate MCP server name: {}", name));
        }
    }
    Ok(())
}

/// Resolve tools for an agent, combining builtin tools and MCP servers
pub fn resolve_tools(
    tools: &[String],
    global_mcp: &[McpServerConfig],
    delegate_mcp: &[McpServerConfig],
    builtin_names: &HashSet<String>,
) -> Result<ResolvedTools> {
    let mut builtins = Vec::new();
    let mut mcp_servers: HashMap<String, (McpServerConfig, Option<Vec<String>>)> = HashMap::new();

    // Combine global and delegate MCP servers
    let all_mcp: HashMap<String, McpServerConfig> = global_mcp
        .iter()
        .chain(delegate_mcp.iter())
        .map(|cfg| (cfg.name().to_string(), cfg.clone()))
        .collect();

    for tool in tools {
        match parse_tool_spec(tool) {
            ToolSpec::Builtin(name) => {
                if !builtin_names.contains(&name) {
                    return Err(anyhow!("Unknown builtin tool: {}", name));
                }
                builtins.push(name);
            }
            ToolSpec::McpAll(mcp_name) => {
                let config = all_mcp
                    .get(&mcp_name)
                    .ok_or_else(|| anyhow!("Unknown MCP server: {}", mcp_name))?;
                mcp_servers.insert(mcp_name.clone(), (config.clone(), None)); // None = all tools
            }
            ToolSpec::McpSpecific(mcp_name, tool_name) => {
                let config = all_mcp
                    .get(&mcp_name)
                    .ok_or_else(|| anyhow!("Unknown MCP server: {}", mcp_name))?;
                if let Some(v) = mcp_servers
                    .entry(mcp_name.clone())
                    .or_insert_with(|| (config.clone(), Some(Vec::new())))
                    .1
                    .as_mut()
                {
                    v.push(tool_name)
                }
            }
        }
    }

    Ok(ResolvedTools {
        builtins,
        mcp_servers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn test_parse_tool_spec() {
        assert_eq!(parse_tool_spec("edit"), ToolSpec::Builtin("edit".into()));
        assert_eq!(
            parse_tool_spec("github.*"),
            ToolSpec::McpAll("github".into())
        );
        assert_eq!(
            parse_tool_spec("github.search_repos"),
            ToolSpec::McpSpecific("github".into(), "search_repos".into())
        );
    }

    #[test]
    fn test_interpolate_env_vars() {
        unsafe {
            std::env::set_var("TEST_VAR", "test_value");
            std::env::set_var("TEST_VAR2", "value2");
        }

        let input = "provider = \"${TEST_VAR}\"\nmodel = \"${TEST_VAR2:-default}\"";
        let result = interpolate_env_vars(input).unwrap();
        assert_eq!(result, "provider = \"test_value\"\nmodel = \"value2\"");

        let with_default = "model = \"${MISSING_VAR:-gpt-4}\"";
        let result = interpolate_env_vars(with_default).unwrap();
        assert_eq!(result, "model = \"gpt-4\"");

        let missing = "model = \"${MISSING_REQUIRED}\"";
        assert!(interpolate_env_vars(missing).is_err());
    }

    /// Helper to deserialize an AgentSettings from a TOML fragment
    fn parse_agent(toml: &str) -> AgentSettings {
        let full = format!(
            "[agent]\nprovider = \"test\"\nmodel = \"test-model\"\n{}",
            toml
        );
        #[derive(Deserialize)]
        struct Wrapper {
            agent: AgentSettings,
        }
        toml::from_str::<Wrapper>(&full)
            .expect("Failed to parse TOML")
            .agent
    }

    fn make_temp_prompt(contents: &str) -> (PathBuf, PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("querymt-prompt-{nanos}"));
        std::fs::create_dir_all(&dir).expect("Failed to create temp prompt dir");
        let file = PathBuf::from("prompt.md");
        std::fs::write(dir.join(&file), contents).expect("Failed to write temp prompt");
        (dir, file)
    }

    #[test]
    fn test_system_absent() {
        let agent = parse_agent("");
        assert!(agent.system.is_empty());
    }

    #[test]
    fn test_system_single_string() {
        let agent = parse_agent("system = \"hello\"");
        assert_eq!(agent.system.len(), 1);
        assert!(matches!(&agent.system[0], SystemPart::Inline(s) if s == "hello"));
    }

    #[test]
    fn test_system_array_of_strings() {
        let agent = parse_agent("system = [\"part1\", \"part2\"]");
        assert_eq!(agent.system.len(), 2);
        assert!(matches!(&agent.system[0], SystemPart::Inline(s) if s == "part1"));
        assert!(matches!(&agent.system[1], SystemPart::Inline(s) if s == "part2"));
    }

    #[test]
    fn test_system_file_reference() {
        let agent = parse_agent("system = [{ file = \"prompts/coder.md\" }]");
        assert_eq!(agent.system.len(), 1);
        assert!(
            matches!(&agent.system[0], SystemPart::File { file } if file == Path::new("prompts/coder.md"))
        );
    }

    #[test]
    fn test_system_mixed_inline_and_file() {
        let agent = parse_agent(
            r#"system = ["You are helpful.", { file = "prompts/rules.md" }, "Be concise."]"#,
        );
        assert_eq!(agent.system.len(), 3);
        assert!(matches!(&agent.system[0], SystemPart::Inline(s) if s == "You are helpful."));
        assert!(
            matches!(&agent.system[1], SystemPart::File { file } if file == Path::new("prompts/rules.md"))
        );
        assert!(matches!(&agent.system[2], SystemPart::Inline(s) if s == "Be concise."));
    }

    #[test]
    fn test_system_multiple_file_references() {
        let agent = parse_agent(
            r#"system = [{ file = "prompts/base.md" }, { file = "prompts/extra.md" }]"#,
        );
        assert_eq!(agent.system.len(), 2);
        assert!(
            matches!(&agent.system[0], SystemPart::File { file } if file == Path::new("prompts/base.md"))
        );
        assert!(
            matches!(&agent.system[1], SystemPart::File { file } if file == Path::new("prompts/extra.md"))
        );
    }

    #[tokio::test]
    async fn test_resolve_system_parts_inline_only() {
        let parts = vec![
            SystemPart::Inline("hello".into()),
            SystemPart::Inline("world".into()),
        ];
        let resolved = resolve_system_parts(&parts, Path::new("."), "test")
            .await
            .unwrap();
        assert_eq!(resolved, vec!["hello", "world"]);
    }

    #[tokio::test]
    async fn test_resolve_system_parts_file_not_found() {
        let parts = vec![SystemPart::File {
            file: PathBuf::from("nonexistent_prompt.md"),
        }];
        let result = resolve_system_parts(&parts, Path::new("."), "test").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to load test prompt")
        );
    }

    #[tokio::test]
    async fn test_resolve_system_parts_file_env_vars() {
        unsafe {
            std::env::set_var("TEST_PROMPT_VAR", "resolved");
        }

        let (dir, file) = make_temp_prompt("Hello ${TEST_PROMPT_VAR}!");
        let parts = vec![SystemPart::File { file }];
        let resolved = resolve_system_parts(&parts, &dir, "test").await.unwrap();
        assert_eq!(resolved, vec!["Hello resolved!"]);
    }

    #[tokio::test]
    async fn test_resolve_system_parts_file_env_default() {
        let (dir, file) = make_temp_prompt("Model ${MISSING_PROMPT_VAR:-gpt-4}");
        let parts = vec![SystemPart::File { file }];
        let resolved = resolve_system_parts(&parts, &dir, "test").await.unwrap();
        assert_eq!(resolved, vec!["Model gpt-4"]);
    }

    #[tokio::test]
    async fn test_resolve_system_parts_file_env_missing() {
        let (dir, file) = make_temp_prompt("${MISSING_PROMPT_REQUIRED}");
        let parts = vec![SystemPart::File { file }];
        let result = resolve_system_parts(&parts, &dir, "test").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_interpolate_toml_value_strings() {
        unsafe {
            std::env::set_var("TOML_TEST_VAR", "interpolated");
        }

        let toml_str = r#"
            provider = "${TOML_TEST_VAR}"
            model = "gpt-4"
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        interpolate_toml_value(&mut value).unwrap();

        let table = value.as_table().unwrap();
        assert_eq!(
            table.get("provider").unwrap().as_str().unwrap(),
            "interpolated"
        );
        assert_eq!(table.get("model").unwrap().as_str().unwrap(), "gpt-4");
    }

    #[test]
    fn test_interpolate_toml_value_arrays() {
        unsafe {
            std::env::set_var("TOML_ARRAY_VAR", "value1");
        }

        let toml_str = r#"
            tools = ["${TOML_ARRAY_VAR}", "tool2"]
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        interpolate_toml_value(&mut value).unwrap();

        let table = value.as_table().unwrap();
        let tools = table.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools[0].as_str().unwrap(), "value1");
        assert_eq!(tools[1].as_str().unwrap(), "tool2");
    }

    #[test]
    fn test_interpolate_toml_value_nested_tables() {
        unsafe {
            std::env::set_var("TOML_NESTED_VAR", "nested_value");
        }

        let toml_str = r#"
            [agent]
            provider = "${TOML_NESTED_VAR}"
            model = "gpt-4"
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        interpolate_toml_value(&mut value).unwrap();

        let table = value.as_table().unwrap();
        let agent = table.get("agent").unwrap().as_table().unwrap();
        assert_eq!(
            agent.get("provider").unwrap().as_str().unwrap(),
            "nested_value"
        );
    }

    #[test]
    fn test_interpolate_toml_value_with_default() {
        let toml_str = r#"
            provider = "${TOML_MISSING_VAR:-default_provider}"
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        interpolate_toml_value(&mut value).unwrap();

        let table = value.as_table().unwrap();
        assert_eq!(
            table.get("provider").unwrap().as_str().unwrap(),
            "default_provider"
        );
    }

    #[test]
    fn test_comments_with_env_vars_full_line() {
        // Full-line comments with ${VAR} should not cause errors
        let toml_str = r#"
            # This is a comment with ${SOME_VAR} that should be ignored
            provider = "anthropic"
            # Another comment: ${ANOTHER_VAR}
            model = "claude-3-5-sonnet-20241022"
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        // Should not error even though SOME_VAR and ANOTHER_VAR are not set
        assert!(interpolate_toml_value(&mut value).is_ok());

        let table = value.as_table().unwrap();
        assert_eq!(
            table.get("provider").unwrap().as_str().unwrap(),
            "anthropic"
        );
        assert_eq!(
            table.get("model").unwrap().as_str().unwrap(),
            "claude-3-5-sonnet-20241022"
        );
    }

    #[test]
    fn test_comments_with_env_vars_inline() {
        // Inline comments with ${VAR} should not cause errors
        let toml_str = r#"
            provider = "anthropic"  # Uses ${API_KEY} for auth
            model = "claude-3-5-sonnet-20241022"  # Or use ${MODEL_OVERRIDE}
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        // Should not error even though API_KEY and MODEL_OVERRIDE are not set
        assert!(interpolate_toml_value(&mut value).is_ok());

        let table = value.as_table().unwrap();
        assert_eq!(
            table.get("provider").unwrap().as_str().unwrap(),
            "anthropic"
        );
        assert_eq!(
            table.get("model").unwrap().as_str().unwrap(),
            "claude-3-5-sonnet-20241022"
        );
    }

    #[test]
    fn test_strings_still_interpolate_with_comments_present() {
        unsafe {
            std::env::set_var("TEST_PROVIDER_VAR", "openai");
            std::env::set_var("TEST_MODEL_VAR", "gpt-4");
        }

        let toml_str = r#"
            # Comment with ${UNSET_VAR}
            provider = "${TEST_PROVIDER_VAR}"  # Another ${COMMENT_VAR}
            model = "${TEST_MODEL_VAR}"
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        interpolate_toml_value(&mut value).unwrap();

        let table = value.as_table().unwrap();
        assert_eq!(table.get("provider").unwrap().as_str().unwrap(), "openai");
        assert_eq!(table.get("model").unwrap().as_str().unwrap(), "gpt-4");
    }

    #[test]
    fn test_mixed_comments_and_interpolation() {
        unsafe {
            std::env::set_var("REAL_VAR", "real_value");
        }

        let toml_str = r#"
            # Top comment ${FAKE_VAR}
            [agent]
            # Section comment ${ANOTHER_FAKE}
            provider = "${REAL_VAR}"  # inline ${INLINE_FAKE}
            model = "test"
            # tools = ["${COMMENTED_OUT_VAR}"]
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        assert!(interpolate_toml_value(&mut value).is_ok());

        let table = value.as_table().unwrap();
        let agent = table.get("agent").unwrap().as_table().unwrap();
        assert_eq!(
            agent.get("provider").unwrap().as_str().unwrap(),
            "real_value"
        );
        assert_eq!(agent.get("model").unwrap().as_str().unwrap(), "test");
    }

    #[test]
    fn test_interpolate_missing_var_in_string_still_errors() {
        let toml_str = r#"
            provider = "${DEFINITELY_MISSING_VAR}"
        "#;
        let mut value: toml::Value = toml::from_str(toml_str).unwrap();
        let result = interpolate_toml_value(&mut value);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("DEFINITELY_MISSING_VAR")
        );
    }
}
