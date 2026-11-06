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
/// **http + OAuth** — the login flow runs through the `_querymt/auth/*`
/// extension using the provider id `mcp:<name>`:
/// ```toml
/// [[mcp]]
/// name = "linear"
/// transport = "http"
/// url = "https://mcp.linear.app/mcp"
/// auth = { type = "oauth", scopes = ["read"] }
/// ```
///
/// **http over a Unix socket** (Unix only; `url` is used for addressing):
/// ```toml
/// [[mcp]]
/// name = "local"
/// transport = "http"
/// url = "http://localhost/mcp"
/// socket_path = "/run/mcp.sock"
/// ```
///
/// Both transports accept `tasks = true` (declare the MCP tasks extension;
/// default `false`) and `cache = false` (disable response caching of tool
/// listings; default `true`).
///
/// To enable MCP tools, add them to `agent.tools`:
/// - `"filesystem.*"` — all tools from the `filesystem` server
/// - `"filesystem.read_file"` — a specific tool from the server
///
/// Build values in Rust with [`McpServerConfig::stdio`] / [`McpServerConfig::http`]
/// and the `with_*` setters; the variants are `#[non_exhaustive]`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "transport", rename_all = "lowercase")]
#[schemars(extend("examples" = [
    {"name": "filesystem", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]},
    {"name": "github", "transport": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}},
    {"name": "context7", "transport": "http", "url": "https://mcp.context7.com/mcp"},
    {"name": "linear", "transport": "http", "url": "https://mcp.linear.app/mcp", "auth": {"type": "oauth", "scopes": ["read"]}},
    {"name": "local", "transport": "http", "url": "http://localhost/mcp", "socket_path": "/run/mcp.sock", "tasks": true, "cache": false}
]))]
pub enum McpServerConfig {
    /// Local process-based MCP server launched via stdio.
    #[serde(rename_all = "snake_case")]
    #[non_exhaustive]
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
        /// Declare the MCP tasks extension to this server (default `false`).
        #[serde(default, skip_serializing_if = "is_false")]
        tasks: bool,
        /// Honour server cache hints for tool listings (default `true`).
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        cache: bool,
    },
    /// Remote HTTP-based MCP server.
    #[serde(rename_all = "snake_case")]
    #[non_exhaustive]
    Http {
        /// Unique name for this server. Referenced in `agent.tools` as `"name.*"`.
        name: String,
        /// Full URL of the MCP HTTP endpoint.
        url: String,
        /// HTTP headers to include in every request (e.g. auth tokens).
        /// Supports `${VAR}` interpolation in values.
        #[serde(default)]
        headers: HashMap<String, String>,
        /// OAuth authorization. Cannot be combined with `socket_path` or an
        /// `Authorization` header.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<McpAuth>,
        /// Connect over this Unix domain socket instead of TCP (Unix only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        socket_path: Option<PathBuf>,
        /// Declare the MCP tasks extension to this server (default `false`).
        #[serde(default, skip_serializing_if = "is_false")]
        tasks: bool,
        /// Honour server cache hints for tool listings (default `true`).
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        cache: bool,
    },
}

/// Authorization scheme for an HTTP MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpAuth {
    /// OAuth 2.1 with discovery, PKCE and (unless `client_id` is set) dynamic
    /// client registration.
    OAuth {
        /// Pre-registered client id. When absent, dynamic registration is used.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        /// Scopes to request.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
        /// Loopback redirect port. Defaults to a fixed port.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redirect_port: Option<u16>,
        /// Refuse grants wider than `scopes` (default `false`).
        ///
        /// Authorization servers may grant more than was requested. With
        /// `strict_scopes = true` a login whose token carries any scope beyond
        /// `scopes` (plus `offline_access`) is discarded, and stored
        /// credentials with such a grant are not used to connect. Checked
        /// against the token response `scope` and, for JWT access tokens, the
        /// `scope` / `scp` claim. Requires a nonempty `scopes`.
        #[serde(default, skip_serializing_if = "is_false")]
        strict_scopes: bool,
    },
}

impl McpAuth {
    /// OAuth with dynamic registration, the default redirect port and the
    /// given scopes.
    pub fn oauth(scopes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::OAuth {
            client_id: None,
            scopes: scopes.into_iter().map(Into::into).collect(),
            redirect_port: None,
            strict_scopes: false,
        }
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn is_true(v: &bool) -> bool {
    *v
}

/// Key in ACP `_meta` under which QueryMT-specific MCP settings travel.
const ACP_META_KEY: &str = "querymt";

/// The settings of [`McpServerConfig`] that ACP's MCP server types cannot
/// express natively. Only non-default values are serialized.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AcpMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    auth: Option<McpAuth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    socket_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "is_false")]
    tasks: bool,
    #[serde(skip_serializing_if = "is_true")]
    cache: bool,
}

impl Default for AcpMeta {
    fn default() -> Self {
        Self {
            auth: None,
            socket_path: None,
            tasks: false,
            cache: true,
        }
    }
}

impl AcpMeta {
    fn is_default(&self) -> bool {
        self.auth.is_none() && self.socket_path.is_none() && !self.tasks && self.cache
    }

    /// Wrap into an ACP `_meta` map, or `None` when everything is default.
    fn into_meta(self) -> Option<serde_json::Map<String, Value>> {
        if self.is_default() {
            return None;
        }
        let value = serde_json::to_value(&self).ok()?;
        Some(serde_json::Map::from_iter([(
            ACP_META_KEY.to_string(),
            value,
        )]))
    }

    /// Read from an ACP `_meta` map; absent keys mean defaults.
    fn from_meta(server_name: &str, meta: Option<&serde_json::Map<String, Value>>) -> Result<Self> {
        match meta.and_then(|m| m.get(ACP_META_KEY)) {
            None | Some(Value::Null) => Ok(Self::default()),
            Some(value) => serde_json::from_value(value.clone()).with_context(|| {
                format!("invalid `_meta.{ACP_META_KEY}` for MCP server `{server_name}`")
            }),
        }
    }
}

impl McpServerConfig {
    /// A stdio server with default settings.
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self::Stdio {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: HashMap::new(),
            tasks: false,
            cache: true,
        }
    }

    /// An HTTP server with default settings.
    pub fn http(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::Http {
            name: name.into(),
            url: url.into(),
            headers: HashMap::new(),
            auth: None,
            socket_path: None,
            tasks: false,
            cache: true,
        }
    }

    /// Set the command arguments (stdio only; ignored for HTTP).
    #[must_use]
    pub fn with_args(mut self, new: impl IntoIterator<Item = impl Into<String>>) -> Self {
        if let Self::Stdio { args, .. } = &mut self {
            *args = new.into_iter().map(Into::into).collect();
        }
        self
    }

    /// Set the process environment (stdio only; ignored for HTTP).
    #[must_use]
    pub fn with_env(mut self, new: HashMap<String, String>) -> Self {
        if let Self::Stdio { env, .. } = &mut self {
            *env = new;
        }
        self
    }

    /// Set the request headers (HTTP only; ignored for stdio).
    #[must_use]
    pub fn with_headers(mut self, new: HashMap<String, String>) -> Self {
        if let Self::Http { headers, .. } = &mut self {
            *headers = new;
        }
        self
    }

    /// Set OAuth authorization (HTTP only; ignored for stdio).
    #[must_use]
    pub fn with_auth(mut self, new: McpAuth) -> Self {
        if let Self::Http { auth, .. } = &mut self {
            *auth = Some(new);
        }
        self
    }

    /// Connect over a Unix domain socket (HTTP only; ignored for stdio).
    #[must_use]
    pub fn with_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        if let Self::Http { socket_path, .. } = &mut self {
            *socket_path = Some(path.into());
        }
        self
    }

    /// Declare the MCP tasks extension to the server.
    #[must_use]
    pub fn with_tasks(mut self, enabled: bool) -> Self {
        let (Self::Stdio { tasks, .. } | Self::Http { tasks, .. }) = &mut self;
        *tasks = enabled;
        self
    }

    /// Enable or disable response caching of tool listings.
    #[must_use]
    pub fn with_cache(mut self, enabled: bool) -> Self {
        let (Self::Stdio { cache, .. } | Self::Http { cache, .. }) = &mut self;
        *cache = enabled;
        self
    }

    /// Get the name of the MCP server
    pub fn name(&self) -> &str {
        match self {
            McpServerConfig::Stdio { name, .. } => name,
            McpServerConfig::Http { name, .. } => name,
        }
    }

    /// The Unix socket this server connects over, if any (HTTP only).
    pub fn socket_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Http { socket_path, .. } => socket_path.as_deref(),
            Self::Stdio { .. } => None,
        }
    }

    /// Whether the MCP tasks extension is declared to this server.
    pub fn tasks_enabled(&self) -> bool {
        let (Self::Stdio { tasks, .. } | Self::Http { tasks, .. }) = self;
        *tasks
    }

    /// Whether response caching of tool listings is enabled.
    pub fn cache_enabled(&self) -> bool {
        let (Self::Stdio { cache, .. } | Self::Http { cache, .. }) = self;
        *cache
    }

    /// Convert to agent-client-protocol McpServer type.
    ///
    /// Settings ACP cannot express travel in `_meta.querymt`; see [`Self::from_acp`].
    pub fn to_acp(&self) -> McpServer {
        match self {
            McpServerConfig::Stdio {
                name,
                command,
                args,
                env,
                tasks,
                cache,
            } => {
                let meta = AcpMeta {
                    tasks: *tasks,
                    cache: *cache,
                    ..AcpMeta::default()
                };
                let server = McpServerStdio::new(name.clone(), PathBuf::from(command))
                    .args(args.clone())
                    .env(
                        env.iter()
                            .map(|(k, v)| EnvVariable::new(k.clone(), v.clone()))
                            .collect(),
                    )
                    .meta(meta.into_meta());
                McpServer::Stdio(server)
            }
            McpServerConfig::Http {
                name,
                url,
                headers,
                auth,
                socket_path,
                tasks,
                cache,
            } => {
                let meta = AcpMeta {
                    auth: auth.clone(),
                    socket_path: socket_path.clone(),
                    tasks: *tasks,
                    cache: *cache,
                };
                let server = McpServerHttp::new(name.clone(), url.clone())
                    .headers(
                        headers
                            .iter()
                            .map(|(k, v)| HttpHeader::new(k.clone(), v.clone()))
                            .collect(),
                    )
                    .meta(meta.into_meta());
                McpServer::Http(server)
            }
        }
    }

    /// Convert from agent-client-protocol McpServer type.
    ///
    /// Reads `_meta.querymt` (`auth`, `socketPath`, `tasks`, `cache`); missing
    /// `_meta` or keys mean defaults.
    ///
    /// # Errors
    /// Fails for transports this version does not know, or when
    /// `_meta.querymt` is malformed.
    pub fn from_acp(server: &McpServer) -> Result<Self> {
        match server {
            McpServer::Stdio(s) => {
                let meta = AcpMeta::from_meta(&s.name, s.meta.as_ref())?;
                Ok(Self::stdio(s.name.clone(), s.command.to_string_lossy())
                    .with_args(s.args.iter().cloned())
                    .with_env(
                        s.env
                            .iter()
                            .map(|e| (e.name.clone(), e.value.clone()))
                            .collect(),
                    )
                    .with_tasks(meta.tasks)
                    .with_cache(meta.cache))
            }
            McpServer::Http(s) => {
                let meta = AcpMeta::from_meta(&s.name, s.meta.as_ref())?;
                let mut config = Self::http(s.name.clone(), s.url.clone())
                    .with_headers(
                        s.headers
                            .iter()
                            .map(|h| (h.name.clone(), h.value.clone()))
                            .collect(),
                    )
                    .with_tasks(meta.tasks)
                    .with_cache(meta.cache);
                if let Some(auth) = meta.auth {
                    config = config.with_auth(auth);
                }
                if let Some(path) = meta.socket_path {
                    config = config.with_socket_path(path);
                }
                Ok(config)
            }
            // McpServer is non-exhaustive, handle unknown variants
            _ => Err(anyhow!("unsupported MCP server transport")),
        }
    }
}
