//! Tool management and permission handling

use crate::agent::core::ToolConfig;

/// Checks if a tool is allowed based on configuration.
///
/// Supports two allowlist entry formats:
/// - `"toolname"` — exact match for built-in tools
/// - `"servername.*"` — wildcard allowing all tools from a given MCP server
///   (use [`is_mcp_tool_allowed_with`] to pass the server name for wildcard resolution)
pub(crate) fn is_tool_allowed_with(config: &ToolConfig, name: &str) -> bool {
    is_mcp_tool_allowed_with(config, name, None)
}

/// Like [`is_tool_allowed_with`] but also accepts the MCP server name so that
/// `"servername.*"` allowlist entries can be matched against MCP tools.
pub(crate) fn is_mcp_tool_allowed_with(
    config: &ToolConfig,
    name: &str,
    server_name: Option<&str>,
) -> bool {
    if config.denylist.contains(name) {
        return false;
    }
    match &config.allowlist {
        None => true,
        Some(allowlist) => {
            // Exact match (covers built-in tools and specific MCP tool names).
            if allowlist.contains(name) {
                return true;
            }
            // Wildcard match: "servername.*" allows all tools from that server.
            if let Some(sname) = server_name {
                let pattern = format!("{}.*", sname);
                if allowlist.contains(pattern.as_str()) {
                    return true;
                }
            }
            false
        }
    }
}
