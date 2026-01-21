//! Configuration file support for agents
//!
//! Supports both single-agent and multi-agent (quorum) configurations from TOML files.

use agent_client_protocol::{
    EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse, McpServerStdio,
};
use anyhow::{Context, Result, anyhow};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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
    pub system: Option<String>,
    pub system_file: Option<PathBuf>,
    #[serde(default)]
    pub parameters: Option<HashMap<String, Value>>,
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
}

fn default_true() -> bool {
    true
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
    pub system: Option<String>,
    pub system_file: Option<PathBuf>,
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
    pub system: Option<String>,
    pub system_file: Option<PathBuf>,
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

/// Load and parse a config file
pub async fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Failed to read config file: {:?}", path))?;

    // Step 1: Interpolate environment variables
    let processed = interpolate_env_vars(&content)?;

    // Step 2: Detect config type and parse
    let config = if processed.contains("[agent]") {
        // Single agent config
        let mut config: SingleAgentConfig =
            toml::from_str(&processed).with_context(|| "Failed to parse single agent config")?;

        // Step 3: Validate
        validate_agent_settings(&config.agent)?;
        validate_mcp_servers(&config.mcp)?;

        // Step 4: Resolve prompt files
        let base_path = path.parent().unwrap_or(Path::new("."));
        resolve_agent_prompts(&mut config.agent, base_path).await?;

        Config::Single(config)
    } else if processed.contains("[quorum]") || processed.contains("[planner]") {
        // Multi-agent config
        let mut config: QuorumConfig =
            toml::from_str(&processed).with_context(|| "Failed to parse quorum config")?;

        // Step 3: Validate
        validate_planner_config(&config.planner)?;
        for delegate in &config.delegates {
            validate_delegate_config(delegate)?;
        }
        validate_mcp_servers(&config.mcp)?;
        for delegate in &config.delegates {
            validate_mcp_servers(&delegate.mcp)?;
        }

        // Step 4: Resolve prompt files
        let base_path = path.parent().unwrap_or(Path::new("."));
        resolve_planner_prompts(&mut config.planner, base_path).await?;
        for delegate in &mut config.delegates {
            resolve_delegate_prompts(delegate, base_path).await?;
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

/// Validate agent settings
fn validate_agent_settings(settings: &AgentSettings) -> Result<()> {
    if settings.system.is_some() && settings.system_file.is_some() {
        return Err(anyhow!(
            "Cannot specify both 'system' and 'system_file' in agent config"
        ));
    }
    Ok(())
}

/// Validate planner config
fn validate_planner_config(config: &PlannerConfig) -> Result<()> {
    if config.system.is_some() && config.system_file.is_some() {
        return Err(anyhow!(
            "Cannot specify both 'system' and 'system_file' in planner config"
        ));
    }
    Ok(())
}

/// Validate delegate config
fn validate_delegate_config(config: &DelegateConfig) -> Result<()> {
    if config.system.is_some() && config.system_file.is_some() {
        return Err(anyhow!(
            "Cannot specify both 'system' and 'system_file' in delegate '{}' config",
            config.id
        ));
    }
    Ok(())
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

/// Resolve and load agent prompt from file
async fn resolve_agent_prompts(settings: &mut AgentSettings, base_path: &Path) -> Result<()> {
    if let Some(file) = &settings.system_file {
        let path = base_path.join(file);
        settings.system = Some(
            tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to load agent prompt from {:?}", path))?,
        );
        settings.system_file = None;
    }
    Ok(())
}

/// Resolve and load planner prompt from file
async fn resolve_planner_prompts(config: &mut PlannerConfig, base_path: &Path) -> Result<()> {
    if let Some(file) = &config.system_file {
        let path = base_path.join(file);
        config.system = Some(
            tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to load planner prompt from {:?}", path))?,
        );
        config.system_file = None;
    }
    Ok(())
}

/// Resolve and load delegate prompt from file
async fn resolve_delegate_prompts(config: &mut DelegateConfig, base_path: &Path) -> Result<()> {
    if let Some(file) = &config.system_file {
        let path = base_path.join(file);
        config.system = Some(tokio::fs::read_to_string(&path).await.with_context(|| {
            format!(
                "Failed to load prompt for delegate '{}' from {:?}",
                config.id, path
            )
        })?);
        config.system_file = None;
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
}
