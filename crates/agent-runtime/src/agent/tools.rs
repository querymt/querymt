//! Tool selection helpers: allow/deny lists share one matching rule
//! (see [`ToolConfig::allows`]).

use crate::agent::core::{ToolConfig, ToolPolicy};
use crate::config::{McpServerConfig, ToolRef, ToolSpec};

/// Whether the session's tool configuration admits MCP server `server`.
///
/// All of these must hold: the policy is not `BuiltInOnly`, the allowlist
/// (when set) contains `server.*`, and the denylist contains neither
/// `server.*` nor the bare `server`. (`ToolConfig::allows` is not reused: a
/// denylist check on a literal `*` tool name would never match `server.*`.)
pub(crate) fn is_mcp_server_visible(config: &ToolConfig, server: &str) -> bool {
    if config.policy == ToolPolicy::BuiltInOnly {
        return false;
    }
    let whole_server = ToolSpec::McpAll(server.to_string());
    if config.deny.covers(&whole_server) || config.deny.contains(ToolRef::builtin(server)) {
        return false;
    }
    config
        .allow
        .as_ref()
        .is_none_or(|allow| allow.covers(&whole_server))
}

/// Tool policy for an agent restricted to `tools`.
///
/// `BuiltInAndProvider` when MCP servers are attached or any selector names
/// MCP tools (`server.tool`, `server.*`) or an attached server by its bare
/// name; otherwise `BuiltInOnly`. Shared by standalone agents, quorum
/// planner/delegates and `.agents` delegation targets.
pub(crate) fn infer_restricted_tool_policy(
    tools: &[ToolSpec],
    mcp_servers: &[McpServerConfig],
) -> ToolPolicy {
    let has_external = tools
        .iter()
        .any(|t| t.is_mcp() || mcp_servers.iter().any(|m| t.is_builtin(m.name())));
    if has_external || !mcp_servers.is_empty() {
        ToolPolicy::BuiltInAndProvider
    } else {
        ToolPolicy::BuiltInOnly
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::core::{ToolConfig, ToolPolicy};
    use crate::config::{ToolRef, ToolSpec};

    fn specs(list: &[&str]) -> Vec<ToolSpec> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn config_with(allow: &[&str]) -> ToolConfig {
        ToolConfig {
            policy: ToolPolicy::BuiltInAndProvider,
            allow: Some(specs(allow).into_iter().collect()),
            deny: Default::default(),
        }
    }

    fn servers(names: &[&str]) -> Vec<crate::config::McpServerConfig> {
        names
            .iter()
            .map(|n| crate::config::McpServerConfig::stdio(*n, "true"))
            .collect()
    }

    fn visible(policy: ToolPolicy, allow: Option<&[&str]>, deny: &[&str]) -> bool {
        let config = ToolConfig {
            policy,
            allow: allow.map(|a| specs(a).into_iter().collect()),
            deny: specs(deny).into_iter().collect(),
        };
        super::is_mcp_server_visible(&config, "docs")
    }

    #[test]
    fn server_visibility_requires_every_condition() {
        use ToolPolicy::*;
        assert!(visible(BuiltInAndProvider, None, &[]));
        assert!(visible(ProviderOnly, None, &[]));
        assert!(!visible(BuiltInOnly, None, &[]));
        // Allowlist must contain `docs.*` (a bare name or one tool is not enough).
        assert!(visible(BuiltInAndProvider, Some(&["skill", "docs.*"]), &[]));
        assert!(!visible(
            BuiltInAndProvider,
            Some(&["skill", "alpha.*"]),
            &[]
        ));
        assert!(!visible(
            BuiltInAndProvider,
            Some(&["skill", "docs.lookup"]),
            &[]
        ));
        assert!(!visible(BuiltInAndProvider, Some(&["skill", "docs"]), &[]));
        // Denylist: `docs.*` or the bare server name hides it; one tool does not.
        assert!(!visible(BuiltInAndProvider, None, &["docs.*"]));
        assert!(!visible(BuiltInAndProvider, None, &["docs"]));
        assert!(visible(BuiltInAndProvider, None, &["docs.lookup"]));
        assert!(visible(BuiltInAndProvider, None, &["other.*"]));
        // Deny wins over allow.
        assert!(!visible(BuiltInAndProvider, Some(&["docs.*"]), &["docs.*"]));
    }

    #[test]
    fn restricted_policy_dot_selector_includes_mcp() {
        let tools = specs(&["read_tool", "docs.*"]);
        assert_eq!(
            super::infer_restricted_tool_policy(&tools, &[]),
            ToolPolicy::BuiltInAndProvider
        );
    }

    #[test]
    fn restricted_policy_bare_attached_server_name_includes_mcp() {
        // Bare selector equal to an attached server's name.
        let tools = specs(&["docs"]);
        assert_eq!(
            super::infer_restricted_tool_policy(&tools, &servers(&["docs"])),
            ToolPolicy::BuiltInAndProvider
        );
    }

    #[test]
    fn restricted_policy_attached_servers_without_selectors() {
        let tools = specs(&["read_tool"]);
        assert_eq!(
            super::infer_restricted_tool_policy(&tools, &servers(&["docs"])),
            ToolPolicy::BuiltInAndProvider
        );
    }

    #[test]
    fn restricted_policy_builtins_only() {
        let tools = specs(&["read_tool", "shell"]);
        assert_eq!(
            super::infer_restricted_tool_policy(&tools, &[]),
            ToolPolicy::BuiltInOnly
        );
    }

    #[test]
    fn specific_mcp_selector_is_server_scoped() {
        let config = config_with(&["alpha.lookup"]);
        // The selected server's tool is admitted...
        assert!(config.allows(ToolRef::mcp("alpha", "lookup")));
        // ...while the same bare tool from another server stays denied, even
        // if it wins the runtime's bare-name tool index (PR #967 regression).
        assert!(!config.allows(ToolRef::mcp("beta", "lookup")));
        // The bare name is not admitted through the built-in path either.
        assert!(!config.allows(ToolRef::builtin("lookup")));
    }

    #[test]
    fn wildcard_selector_is_server_scoped() {
        let config = config_with(&["alpha.*"]);
        assert!(config.allows(ToolRef::mcp("alpha", "lookup")));
        assert!(config.allows(ToolRef::mcp("alpha", "anything")));
        assert!(!config.allows(ToolRef::mcp("beta", "lookup")));
        assert!(!config.allows(ToolRef::builtin("lookup")));
    }

    #[test]
    fn bare_builtin_entry_does_not_admit_mcp_tool() {
        let config = config_with(&["lookup"]);
        assert!(config.allows(ToolRef::builtin("lookup")));
        assert!(!config.allows(ToolRef::mcp("alpha", "lookup")));
    }

    #[test]
    fn deny_wins_over_allow_with_the_same_rule() {
        let mut config = config_with(&["alpha.*"]);
        config.deny = specs(&["alpha.lookup"]).into_iter().collect();
        assert!(!config.allows(ToolRef::mcp("alpha", "lookup")));
        assert!(config.allows(ToolRef::mcp("alpha", "other")));
    }

    #[test]
    fn bare_deny_does_not_hide_an_mcp_tool() {
        let config = ToolConfig {
            policy: ToolPolicy::BuiltInAndProvider,
            allow: None,
            deny: specs(&["lookup"]).into_iter().collect(),
        };
        assert!(!config.allows(ToolRef::builtin("lookup")));
        assert!(config.allows(ToolRef::mcp("alpha", "lookup")));
    }

    #[test]
    fn server_wildcard_deny_hides_every_tool_of_the_server() {
        let config = ToolConfig {
            policy: ToolPolicy::BuiltInAndProvider,
            allow: None,
            deny: specs(&["github.*"]).into_iter().collect(),
        };
        assert!(!config.allows(ToolRef::mcp("github", "create_issue")));
        assert!(config.allows(ToolRef::mcp("gitlab", "create_issue")));
    }

    #[test]
    fn missing_allowlist_admits_all_tools() {
        let config = ToolConfig::default();
        assert!(config.allows(ToolRef::builtin("shell")));
        assert!(config.allows(ToolRef::mcp("alpha", "lookup")));
    }
}
