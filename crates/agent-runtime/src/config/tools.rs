use super::*;
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

/// Reason a tool selector string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolSpecError {
    #[error("tool selector is empty")]
    Empty,
    #[error("invalid tool selector `{selector}`: empty server or tool name")]
    EmptySegment { selector: String },
    #[error("invalid tool selector `{selector}`: `*` is only allowed as `server.*`")]
    MisplacedWildcard { selector: String },
}

/// A typed tool selector, parsed once when configuration is read.
///
/// - `shell` selects a built-in or provider tool by bare name;
/// - `server.tool` selects one MCP tool;
/// - `server.*` selects every tool of an MCP server.
///
/// A bare name never matches an MCP tool (see [`ToolSet::contains`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ToolSpec {
    /// `"edit"`
    Builtin(String),
    /// `"github.*"`
    McpAll(String),
    /// `"github.search_repos"`
    McpSpecific(String, String),
}

impl ToolSpec {
    /// The bare tool name when this selects a non-MCP tool.
    pub fn as_builtin(&self) -> Option<&str> {
        match self {
            Self::Builtin(name) => Some(name),
            _ => None,
        }
    }

    /// Whether this selects a non-MCP tool called `name`.
    pub fn is_builtin(&self, name: &str) -> bool {
        self.as_builtin() == Some(name)
    }

    /// Whether this selects MCP tools (`server.tool` or `server.*`).
    pub fn is_mcp(&self) -> bool {
        !matches!(self, Self::Builtin(_))
    }

    /// The MCP server this selector is scoped to, if any.
    pub fn server(&self) -> Option<&str> {
        match self {
            Self::Builtin(_) => None,
            Self::McpAll(server) | Self::McpSpecific(server, _) => Some(server),
        }
    }
}

impl FromStr for ToolSpec {
    type Err = ToolSpecError;

    fn from_str(selector: &str) -> Result<Self, Self::Err> {
        if selector.is_empty() {
            return Err(ToolSpecError::Empty);
        }
        let bad = |kind: fn(String) -> ToolSpecError| kind(selector.to_string());
        let empty_segment = |selector: String| ToolSpecError::EmptySegment { selector };
        let wildcard = |selector: String| ToolSpecError::MisplacedWildcard { selector };

        match selector.split_once('.') {
            None if selector.contains('*') => Err(bad(wildcard)),
            None => Ok(Self::Builtin(selector.to_string())),
            Some((server, tool)) => {
                if server.is_empty() || tool.is_empty() {
                    Err(bad(empty_segment))
                } else if server.contains('*') || (tool.contains('*') && tool != "*") {
                    Err(bad(wildcard))
                } else if tool == "*" {
                    Ok(Self::McpAll(server.to_string()))
                } else {
                    Ok(Self::McpSpecific(server.to_string(), tool.to_string()))
                }
            }
        }
    }
}

impl TryFrom<String> for ToolSpec {
    type Error = ToolSpecError;

    fn try_from(selector: String) -> Result<Self, Self::Error> {
        selector.parse()
    }
}

impl fmt::Display for ToolSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin(name) => f.write_str(name),
            Self::McpAll(server) => write!(f, "{server}.*"),
            Self::McpSpecific(server, tool) => write!(f, "{server}.{tool}"),
        }
    }
}

impl From<ToolSpec> for String {
    fn from(spec: ToolSpec) -> Self {
        spec.to_string()
    }
}

impl JsonSchema for ToolSpec {
    fn schema_name() -> Cow<'static, str> {
        "ToolSpec".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "description": "Tool selector: `name` (built-in or provider tool), `server.tool` (one MCP tool) or `server.*` (every tool of an MCP server).",
            "examples": ["shell", "github.create_issue", "ibkr.*"]
        })
    }
}

/// Borrowed identity of a concrete tool call: its name and, for MCP tools, the
/// serving MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolRef<'a> {
    pub name: &'a str,
    pub server: Option<&'a str>,
}

impl<'a> ToolRef<'a> {
    /// A non-MCP (built-in or provider) tool.
    pub const fn builtin(name: &'a str) -> Self {
        Self { name, server: None }
    }

    /// A tool served by MCP server `server`.
    pub const fn mcp(server: &'a str, name: &'a str) -> Self {
        Self {
            name,
            server: Some(server),
        }
    }
}

impl fmt::Display for ToolRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.server {
            Some(server) => write!(f, "{server}.{}", self.name),
            None => f.write_str(self.name),
        }
    }
}

/// Which tools of one MCP server a [`ToolSet`] covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerTools {
    All,
    Only(HashSet<String>),
}

/// A set of tool selectors with one matching rule: a bare selector matches only
/// non-MCP tools, `server.tool` matches that tool of that server, and
/// `server.*` matches every tool of the server. Lookups borrow `&str`, so
/// checking a call never allocates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolSet {
    builtin: HashSet<String>,
    mcp: HashMap<String, ServerTools>,
}

impl ToolSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.builtin.is_empty() && self.mcp.is_empty()
    }

    /// Whether the concrete call `tool` is matched by this set.
    pub fn contains(&self, tool: ToolRef<'_>) -> bool {
        match tool.server {
            None => self.builtin.contains(tool.name),
            Some(server) => match self.mcp.get(server) {
                Some(ServerTools::All) => true,
                Some(ServerTools::Only(names)) => names.contains(tool.name),
                None => false,
            },
        }
    }

    /// Whether every tool `spec` selects is matched by this set.
    pub fn covers(&self, spec: &ToolSpec) -> bool {
        match spec {
            ToolSpec::Builtin(name) => self.builtin.contains(name),
            ToolSpec::McpAll(server) => matches!(self.mcp.get(server), Some(ServerTools::All)),
            ToolSpec::McpSpecific(server, name) => self.contains(ToolRef::mcp(server, name)),
        }
    }

    /// Records one concrete tool (used for session grants).
    pub fn insert(&mut self, tool: ToolRef<'_>) {
        match tool.server {
            None => {
                if !self.builtin.contains(tool.name) {
                    self.builtin.insert(tool.name.to_string());
                }
            }
            Some(server) => {
                if self.contains(tool) {
                    return;
                }
                match self.mcp.get_mut(server) {
                    Some(ServerTools::Only(names)) => {
                        names.insert(tool.name.to_string());
                    }
                    Some(ServerTools::All) => {}
                    None => {
                        self.mcp.insert(
                            server.to_string(),
                            ServerTools::Only(HashSet::from([tool.name.to_string()])),
                        );
                    }
                }
            }
        }
    }

    fn insert_spec(&mut self, spec: ToolSpec) {
        match spec {
            ToolSpec::Builtin(name) => {
                self.builtin.insert(name);
            }
            ToolSpec::McpAll(server) => {
                self.mcp.insert(server, ServerTools::All);
            }
            ToolSpec::McpSpecific(server, name) => {
                match self
                    .mcp
                    .entry(server)
                    .or_insert_with(|| ServerTools::Only(HashSet::new()))
                {
                    ServerTools::Only(names) => {
                        names.insert(name);
                    }
                    ServerTools::All => {}
                }
            }
        }
    }

    /// Names of MCP servers this set mentions.
    pub fn servers(&self) -> impl Iterator<Item = &str> {
        self.mcp.keys().map(String::as_str)
    }

    /// The selectors this set holds, in a deterministic order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .builtin
            .iter()
            .cloned()
            .map(ToolSpec::Builtin)
            .collect();
        for (server, tools) in &self.mcp {
            match tools {
                ServerTools::All => specs.push(ToolSpec::McpAll(server.clone())),
                ServerTools::Only(names) => specs.extend(
                    names
                        .iter()
                        .map(|name| ToolSpec::McpSpecific(server.clone(), name.clone())),
                ),
            }
        }
        specs.sort_by_key(ToString::to_string);
        specs
    }
}

impl FromIterator<ToolSpec> for ToolSet {
    fn from_iter<I: IntoIterator<Item = ToolSpec>>(iter: I) -> Self {
        let mut set = Self::new();
        set.extend(iter);
        set
    }
}

impl Extend<ToolSpec> for ToolSet {
    fn extend<I: IntoIterator<Item = ToolSpec>>(&mut self, iter: I) {
        for spec in iter {
            self.insert_spec(spec);
        }
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
    /// Local tool names stay bare selectors. A wildcard MCP selection becomes
    /// `server.*` and a specific selection stays `server.tool`, so
    /// authorization is bound to the originating MCP server (MCP tools are
    /// advertised under bare provider names that may collide across servers).
    ///
    /// Servers are emitted in sorted order so the list is deterministic
    /// despite `mcp_servers` being a `HashMap`.
    pub fn runtime_tool_selectors(&self) -> Vec<ToolSpec> {
        let mut selectors: Vec<ToolSpec> = self
            .local_tools
            .iter()
            .cloned()
            .map(ToolSpec::Builtin)
            .collect();
        let mut servers: Vec<(&String, &Option<Vec<String>>)> = self
            .mcp_servers
            .iter()
            .map(|(server, (_, tools))| (server, tools))
            .collect();
        servers.sort_by_key(|(server, _)| *server);
        for (server, tools) in servers {
            match tools {
                None => selectors.push(ToolSpec::McpAll(server.clone())),
                Some(names) => selectors.extend(
                    names
                        .iter()
                        .map(|name| ToolSpec::McpSpecific(server.clone(), name.clone())),
                ),
            }
        }
        selectors
    }
}

/// Resolve local and MCP tool specifications for an agent.
///
/// # Errors
///
/// Fails when a built-in selector names an unknown local tool or an MCP
/// selector names a server that is not configured.
pub fn resolve_tools(
    tools: &[ToolSpec],
    global_mcp: &[McpServerConfig],
    delegate_mcp: &[McpServerConfig],
    local_tool_names: &HashSet<String>,
) -> Result<ResolvedTools> {
    let mut local_tools = Vec::new();
    let mut mcp_servers: HashMap<String, (McpServerConfig, Option<Vec<String>>)> = HashMap::new();

    // Combine global and delegate MCP servers
    let all_mcp: HashMap<&str, &McpServerConfig> = global_mcp
        .iter()
        .chain(delegate_mcp.iter())
        .map(|cfg| (cfg.name(), cfg))
        .collect();

    for tool in tools {
        match tool {
            ToolSpec::Builtin(name) => {
                if !local_tool_names.contains(name) {
                    return Err(anyhow!("Unknown local tool: {name}"));
                }
                local_tools.push(name.clone());
            }
            ToolSpec::McpAll(mcp_name) => {
                let config = all_mcp
                    .get(mcp_name.as_str())
                    .ok_or_else(|| anyhow!("Unknown MCP server: {mcp_name}"))?;
                // `None` = all tools.
                mcp_servers.insert(mcp_name.clone(), ((*config).clone(), None));
            }
            ToolSpec::McpSpecific(mcp_name, tool_name) => {
                let config = all_mcp
                    .get(mcp_name.as_str())
                    .ok_or_else(|| anyhow!("Unknown MCP server: {mcp_name}"))?;
                if let Some(names) = mcp_servers
                    .entry(mcp_name.clone())
                    .or_insert_with(|| ((*config).clone(), Some(Vec::new())))
                    .1
                    .as_mut()
                {
                    names.push(tool_name.clone());
                }
            }
        }
    }

    Ok(ResolvedTools {
        local_tools,
        mcp_servers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> ToolSpec {
        s.parse().unwrap()
    }

    #[test]
    fn parses_the_three_forms() {
        assert_eq!(spec("edit"), ToolSpec::Builtin("edit".into()));
        assert_eq!(spec("github.*"), ToolSpec::McpAll("github".into()));
        assert_eq!(
            spec("github.search_repos"),
            ToolSpec::McpSpecific("github".into(), "search_repos".into())
        );
    }

    #[test]
    fn display_round_trips() {
        for text in ["shell", "ibkr.*", "github.create_issue"] {
            assert_eq!(spec(text).to_string(), text);
        }
    }

    #[test]
    fn rejects_malformed_selectors() {
        for bad in ["", "a.", ".b", "a.*.b", "*", "a*", "*.b", "a.b*"] {
            assert!(bad.parse::<ToolSpec>().is_err(), "{bad:?} must be rejected");
        }
        assert_eq!("".parse::<ToolSpec>(), Err(ToolSpecError::Empty));
    }

    #[test]
    fn serde_goes_through_the_string_form() {
        let specs: Vec<ToolSpec> = serde_json::from_str(r#"["shell","a.*","a.b"]"#).unwrap();
        assert_eq!(
            serde_json::to_string(&specs).unwrap(),
            r#"["shell","a.*","a.b"]"#
        );
        let err = serde_json::from_str::<Vec<ToolSpec>>(r#"["a."]"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("a."), "{err}");
    }

    #[test]
    fn schema_is_a_string() {
        let schema = schemars::schema_for!(ToolSpec);
        assert_eq!(schema.as_value()["type"], "string");
    }

    #[test]
    fn bare_selector_never_matches_an_mcp_tool() {
        let set: ToolSet = [spec("lookup")].into_iter().collect();
        assert!(set.contains(ToolRef::builtin("lookup")));
        assert!(!set.contains(ToolRef::mcp("alpha", "lookup")));
    }

    #[test]
    fn server_wildcard_matches_every_tool_of_that_server_only() {
        let set: ToolSet = [spec("ibkr.*")].into_iter().collect();
        assert!(set.contains(ToolRef::mcp("ibkr", "place_order")));
        assert!(!set.contains(ToolRef::mcp("other", "place_order")));
        assert!(!set.contains(ToolRef::builtin("place_order")));
    }

    #[test]
    fn same_tool_name_on_two_servers_is_distinguished() {
        let set: ToolSet = [spec("alpha.lookup")].into_iter().collect();
        assert!(set.contains(ToolRef::mcp("alpha", "lookup")));
        assert!(!set.contains(ToolRef::mcp("beta", "lookup")));
    }

    #[test]
    fn covers_respects_wildcards() {
        let set: ToolSet = [spec("a.*"), spec("b.x"), spec("shell")]
            .into_iter()
            .collect();
        assert!(set.covers(&spec("a.*")) && set.covers(&spec("a.t")));
        assert!(set.covers(&spec("b.x")) && !set.covers(&spec("b.*")));
        assert!(set.covers(&spec("shell")) && !set.covers(&spec("edit")));
    }

    #[test]
    fn insert_records_exact_server_qualified_tools() {
        let mut set = ToolSet::new();
        set.insert(ToolRef::mcp("alpha", "lookup"));
        set.insert(ToolRef::builtin("shell"));
        assert!(set.contains(ToolRef::mcp("alpha", "lookup")));
        assert!(!set.contains(ToolRef::mcp("beta", "lookup")));
        assert!(!set.contains(ToolRef::mcp("alpha", "other")));
        assert!(set.contains(ToolRef::builtin("shell")));
    }
}
