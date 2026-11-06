//! Tool selection tests: allow/deny lists share one matching rule
//! (see [`ToolConfig::allows`]).

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
