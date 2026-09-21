//! Tool management and permission handling

use crate::agent::core::ToolConfig;

/// Checks if a non-MCP tool is allowed based on configuration.
///
/// Bare allowlist entries (e.g. `"toolname"`) match built-in tools by exact
/// name. MCP tools are deliberately *not* admitted through this path — use
/// [`is_mcp_tool_allowed_with`] so that server-qualified entries
/// (`"servername.tool"` / `"servername.*"`) are honoured and a bare entry can
/// never authorize an MCP tool.
pub(crate) fn is_tool_allowed_with(config: &ToolConfig, name: &str) -> bool {
    is_mcp_tool_allowed_with(config, name, None)
}

/// Like [`is_tool_allowed_with`] but also accepts the MCP server name so that
/// server-qualified allowlist entries can be matched against MCP tools.
///
/// MCP tools are advertised under bare provider names that can collide across
/// servers, so when `server_name` is `Some` only qualified entries match:
///
/// - `"servername.tool"` — exact match for a specific MCP tool selection
/// - `"servername.*"` — wildcard allowing all tools from that server
///
/// A bare `"tool"` entry never admits an MCP tool, so selecting `alpha.lookup`
/// cannot authorize `beta.lookup` when multiple attached servers expose the
/// same provider tool name.
pub(crate) fn is_mcp_tool_allowed_with(
    config: &ToolConfig,
    name: &str,
    server_name: Option<&str>,
) -> bool {
    if config.denylist.contains(name) {
        return false;
    }
    let Some(allowlist) = &config.allowlist else {
        return true;
    };
    let Some(server) = server_name else {
        // Non-MCP path: bare exact match covers built-in tools.
        return allowlist.contains(name);
    };
    // MCP tools are advertised under bare names but must be authorized with
    // server-qualified entries so the originating server is part of the
    // authorization decision (exact selection first, then the wildcard).
    if allowlist.contains(format!("{server}.{name}").as_str()) {
        return true;
    }
    allowlist.contains(format!("{server}.*").as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::core::ToolPolicy;
    use std::collections::HashSet;

    fn config_with(allow: &[&str]) -> ToolConfig {
        ToolConfig {
            policy: ToolPolicy::BuiltInAndProvider,
            allowlist: Some(allow.iter().map(|s| (*s).to_string()).collect()),
            denylist: HashSet::new(),
        }
    }

    #[test]
    fn specific_mcp_selector_is_server_scoped() {
        let config = config_with(&["alpha.lookup"]);
        // The selected server's tool is admitted...
        assert!(is_mcp_tool_allowed_with(&config, "lookup", Some("alpha")));
        // ...while the same bare tool from another server stays denied, even
        // if it wins the runtime's bare-name tool index (PR #967 regression).
        assert!(!is_mcp_tool_allowed_with(&config, "lookup", Some("beta")));
        // The bare name is not admitted through the built-in path either.
        assert!(!is_tool_allowed_with(&config, "lookup"));
    }

    #[test]
    fn wildcard_selector_is_server_scoped() {
        let config = config_with(&["alpha.*"]);
        assert!(is_mcp_tool_allowed_with(&config, "lookup", Some("alpha")));
        assert!(is_mcp_tool_allowed_with(&config, "anything", Some("alpha")));
        assert!(!is_mcp_tool_allowed_with(&config, "lookup", Some("beta")));
        assert!(!is_tool_allowed_with(&config, "lookup"));
    }

    #[test]
    fn bare_builtin_entry_does_not_admit_mcp_tool() {
        let config = config_with(&["lookup"]);
        // Bare entries keep admitting the built-in tool...
        assert!(is_tool_allowed_with(&config, "lookup"));
        // ...but never an MCP tool that happens to share the name.
        assert!(!is_mcp_tool_allowed_with(&config, "lookup", Some("alpha")));
    }

    #[test]
    fn denylist_stops_mcp_tools_before_allowlist() {
        let mut config = config_with(&["alpha.*"]);
        config.denylist.insert("lookup".to_string());
        assert!(!is_mcp_tool_allowed_with(&config, "lookup", Some("alpha")));
        assert!(is_mcp_tool_allowed_with(&config, "other", Some("alpha")));
    }

    #[test]
    fn missing_allowlist_admits_all_tools() {
        let config = ToolConfig {
            policy: ToolPolicy::BuiltInAndProvider,
            allowlist: None,
            denylist: HashSet::new(),
        };
        assert!(is_tool_allowed_with(&config, "shell"));
        assert!(is_mcp_tool_allowed_with(&config, "lookup", Some("alpha")));
    }
}
