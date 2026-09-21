use super::*;

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

/// Resolved tools for an agent.
#[derive(Debug, Clone)]
pub struct ResolvedTools {
    pub local_tools: Vec<String>,
    pub mcp_servers: HashMap<String, (McpServerConfig, Option<Vec<String>>)>,
}

impl ResolvedTools {
    /// Normalized tool selectors for the runtime tool list and allowlist.
    ///
    /// Local tool names pass through unchanged. Resolved MCP selections are
    /// rendered in the form the runtime understands: a wildcard selection
    /// becomes `server.*` and a specific selection stays `server.tool`, so
    /// authorization is bound to the originating MCP server. This matters
    /// because MCP tools are advertised under bare provider names that may
    /// collide across servers; filtering matches qualified entries through
    /// `crate::agent::tools::is_mcp_tool_allowed_with`.
    ///
    /// Servers are emitted in sorted order so the list is deterministic
    /// despite `mcp_servers` being a `HashMap`.
    pub fn runtime_tool_selectors(&self) -> Vec<String> {
        let mut selectors = self.local_tools.clone();
        let mut servers: Vec<(&String, &Option<Vec<String>>)> = self
            .mcp_servers
            .iter()
            .map(|(server, (_, tools))| (server, tools))
            .collect();
        servers.sort_by_key(|(server, _)| *server);
        for (server, tools) in servers {
            match tools {
                None => selectors.push(format!("{server}.*")),
                Some(names) => {
                    selectors.extend(names.iter().map(|name| format!("{server}.{name}")))
                }
            }
        }
        selectors
    }
}

/// Resolve local and MCP tool specifications for an agent.
pub fn resolve_tools(
    tools: &[String],
    global_mcp: &[McpServerConfig],
    delegate_mcp: &[McpServerConfig],
    local_tool_names: &HashSet<String>,
) -> Result<ResolvedTools> {
    let mut local_tools = Vec::new();
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
                if !local_tool_names.contains(&name) {
                    return Err(anyhow!("Unknown local tool: {}", name));
                }
                local_tools.push(name);
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
        local_tools,
        mcp_servers,
    })
}
