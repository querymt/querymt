use super::*;

/// MCP (Model Context Protocol) server configuration.
///
/// The `transport` field is the discriminator — it must be `"stdio"` or `"http"`.
/// Note: the field is named `transport`, NOT `type`.
///
/// **stdio** — spawns a local process:
/// ```toml
/// [[mcp]]
/// name = "filesystem"
/// transport = "stdio"
/// command = "npx"
/// args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
/// ```
///
/// **http** — connects to a remote MCP endpoint:
/// ```toml
/// [[mcp]]
/// name = "context7"
/// transport = "http"
/// url = "https://mcp.context7.com/mcp"
/// headers = { AUTHORIZATION = "Bearer ${CONTEXT7_API_KEY}" }
/// ```
///
/// To enable MCP tools, add them to `agent.tools`:
/// - `"filesystem.*"` — all tools from the `filesystem` server
/// - `"filesystem.read_file"` — a specific tool from the server
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "transport", rename_all = "lowercase")]
#[schemars(extend("examples" = [
    {"name": "filesystem", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
    {"name": "github", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}},
    {"name": "context7", "transport": "http", "url": "https://mcp.context7.com/mcp"}
]))]
pub enum McpServerConfig {
    /// Local process-based MCP server launched via stdio.
    #[serde(rename_all = "snake_case")]
    Stdio {
        /// Unique name for this server. Referenced in `agent.tools` as `"name.*"`.
        name: String,
        /// Executable to run (e.g. `"npx"`, `"uvx"`, `"/path/to/binary"`).
        command: String,
        /// Arguments passed to the command.
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables to set for the server process.
        /// Supports `${VAR}` interpolation.
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// Remote HTTP-based MCP server.
    #[serde(rename_all = "snake_case")]
    Http {
        /// Unique name for this server. Referenced in `agent.tools` as `"name.*"`.
        name: String,
        /// Full URL of the MCP HTTP endpoint.
        url: String,
        /// HTTP headers to include in every request (e.g. auth tokens).
        /// Supports `${VAR}` interpolation in values.
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
            // McpServer is non-exhaustive, handle unknown variants
            _ => panic!("Unknown MCP server transport type"),
        }
    }
}
