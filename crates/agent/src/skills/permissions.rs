use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Skill-level permissions in agent config
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct SkillPermissions {
    /// Pattern-based permissions: "skill-name" or "prefix-*"
    pub patterns: HashMap<String, PermissionLevel>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PermissionLevel {
    /// Automatically allow (default)
    #[default]
    Allow,
    /// Block entirely
    Deny,
    /// Prompt user for confirmation
    Ask,
}

impl SkillPermissions {
    /// Resolve a configured entry for `name`: an exact entry, else the
    /// matching prefix wildcard (`prefix*`, other than `*`) with the longest
    /// prefix. Keys are unique, so the result is deterministic.
    fn resolve(&self, name: &str) -> Option<PermissionLevel> {
        if let Some(&level) = self.patterns.get(name) {
            return Some(level);
        }
        self.patterns
            .iter()
            .filter_map(|(pattern, &level)| {
                let prefix = pattern.strip_suffix('*')?;
                (!prefix.is_empty() && name.starts_with(prefix)).then_some((prefix.len(), level))
            })
            .max_by_key(|(len, _)| *len)
            .map(|(_, level)| level)
    }

    /// Check permission level for a local skill name.
    ///
    /// Exact entry, else the longest matching prefix wildcard, else the
    /// catch-all `"*"`, else `Allow`.
    pub fn check(&self, skill_name: &str) -> PermissionLevel {
        self.resolve(skill_name)
            .or_else(|| self.patterns.get("*").copied())
            .unwrap_or(PermissionLevel::Allow)
    }

    /// Check permission level for a skill published by an MCP server, matched
    /// against `server.name`.
    ///
    /// Exact entry, else the longest matching prefix wildcard (`docs.*`),
    /// else `"*" = "deny"` denies. A catch-all `allow` or `ask` does not
    /// apply: untrusted server content is never admitted silently, so the
    /// default is `Ask`.
    pub fn check_mcp(&self, server: &str, name: &str) -> PermissionLevel {
        self.resolve(&format!("{server}.{name}"))
            .or_else(|| {
                (self.patterns.get("*") == Some(&PermissionLevel::Deny))
                    .then_some(PermissionLevel::Deny)
            })
            .unwrap_or(PermissionLevel::Ask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        let mut perms = SkillPermissions::default();
        perms
            .patterns
            .insert("test-skill".to_string(), PermissionLevel::Deny);

        assert_eq!(perms.check("test-skill"), PermissionLevel::Deny);
        assert_eq!(perms.check("other-skill"), PermissionLevel::Allow);
    }

    #[test]
    fn test_wildcard_match() {
        let mut perms = SkillPermissions::default();
        perms
            .patterns
            .insert("internal-*".to_string(), PermissionLevel::Deny);
        perms
            .patterns
            .insert("experimental-*".to_string(), PermissionLevel::Ask);

        assert_eq!(perms.check("internal-debug"), PermissionLevel::Deny);
        assert_eq!(perms.check("experimental-feature"), PermissionLevel::Ask);
        assert_eq!(perms.check("public-skill"), PermissionLevel::Allow);
    }

    #[test]
    fn test_catch_all() {
        let mut perms = SkillPermissions::default();
        perms
            .patterns
            .insert("*".to_string(), PermissionLevel::Deny);
        perms
            .patterns
            .insert("safe-skill".to_string(), PermissionLevel::Allow);

        assert_eq!(perms.check("safe-skill"), PermissionLevel::Allow);
        assert_eq!(perms.check("other-skill"), PermissionLevel::Deny);
    }

    #[test]
    fn test_default_allow() {
        let perms = SkillPermissions::default();
        assert_eq!(perms.check("any-skill"), PermissionLevel::Allow);
    }

    #[test]
    fn test_exact_takes_precedence() {
        let mut perms = SkillPermissions::default();
        perms
            .patterns
            .insert("test-*".to_string(), PermissionLevel::Deny);
        perms
            .patterns
            .insert("test-allowed".to_string(), PermissionLevel::Allow);

        // Exact match should take precedence over wildcard
        assert_eq!(perms.check("test-allowed"), PermissionLevel::Allow);
        assert_eq!(perms.check("test-denied"), PermissionLevel::Deny);
    }

    #[test]
    fn test_empty_pattern() {
        let perms = SkillPermissions::default();
        assert_eq!(perms.check(""), PermissionLevel::Allow);
    }

    #[test]
    fn test_overlapping_wildcards_use_the_longest_prefix() {
        for _ in 0..50 {
            let mut perms = SkillPermissions::default();
            perms
                .patterns
                .insert("team-*".to_string(), PermissionLevel::Allow);
            perms
                .patterns
                .insert("team-secret-*".to_string(), PermissionLevel::Deny);
            perms
                .patterns
                .insert("t*".to_string(), PermissionLevel::Ask);
            assert_eq!(perms.check("team-secret-x"), PermissionLevel::Deny);
            assert_eq!(perms.check("team-x"), PermissionLevel::Allow);
            assert_eq!(perms.check("tools"), PermissionLevel::Ask);
        }
    }

    #[test]
    fn test_mcp_unconfigured_asks_and_catch_all_allow_does_not_admit() {
        let mut perms = SkillPermissions::default();
        assert_eq!(perms.check_mcp("docs", "triage"), PermissionLevel::Ask);
        perms.patterns.insert("*".into(), PermissionLevel::Allow);
        assert_eq!(perms.check_mcp("docs", "triage"), PermissionLevel::Ask);
        perms.patterns.insert("*".into(), PermissionLevel::Ask);
        assert_eq!(perms.check_mcp("docs", "triage"), PermissionLevel::Ask);
        // Local behaviour is unchanged.
        assert_eq!(perms.check("local"), PermissionLevel::Ask);
    }

    #[test]
    fn test_mcp_catch_all_deny_and_server_wildcards() {
        let mut perms = SkillPermissions::default();
        perms.patterns.insert("*".into(), PermissionLevel::Deny);
        assert_eq!(perms.check_mcp("docs", "triage"), PermissionLevel::Deny);
        perms
            .patterns
            .insert("docs.*".into(), PermissionLevel::Allow);
        assert_eq!(perms.check_mcp("docs", "triage"), PermissionLevel::Allow);
        assert_eq!(perms.check_mcp("ops", "triage"), PermissionLevel::Deny);
        perms
            .patterns
            .insert("docs.secret".into(), PermissionLevel::Deny);
        assert_eq!(perms.check_mcp("docs", "secret"), PermissionLevel::Deny);

        let mut perms = SkillPermissions::default();
        perms
            .patterns
            .insert("docs.*".into(), PermissionLevel::Deny);
        assert_eq!(perms.check_mcp("docs", "x"), PermissionLevel::Deny);
        assert_eq!(perms.check_mcp("ops", "x"), PermissionLevel::Ask);
    }
}
