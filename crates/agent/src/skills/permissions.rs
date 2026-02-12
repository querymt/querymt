use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Skill-level permissions in agent config
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SkillPermissions {
    /// Pattern-based permissions: "skill-name" or "prefix-*"
    pub patterns: HashMap<String, PermissionLevel>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
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
    /// Check permission level for a skill name
    pub fn check(&self, skill_name: &str) -> PermissionLevel {
        // Check exact match first
        if let Some(&level) = self.patterns.get(skill_name) {
            return level;
        }

        // Check wildcard patterns
        for (pattern, &level) in &self.patterns {
            if Self::matches_pattern(pattern, skill_name) {
                return level;
            }
        }

        // Check for catch-all "*"
        self.patterns
            .get("*")
            .copied()
            .unwrap_or(PermissionLevel::Allow)
    }

    fn matches_pattern(pattern: &str, name: &str) -> bool {
        if let Some(prefix) = pattern.strip_suffix('*') {
            name.starts_with(prefix)
        } else {
            pattern == name
        }
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
}
