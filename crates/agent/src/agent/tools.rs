//! Tool management and permission handling

use crate::agent::core::ToolConfig;

/// Checks if a tool is allowed based on configuration.
pub(crate) fn is_tool_allowed_with(config: &ToolConfig, name: &str) -> bool {
    if config.denylist.contains(name) {
        return false;
    }
    match &config.allowlist {
        Some(allowlist) => allowlist.contains(name),
        None => true,
    }
}
