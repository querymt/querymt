use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// A loaded skill with its metadata and content
#[derive(Debug, Clone)]
pub struct Skill {
    /// Root directory containing the skill
    pub path: PathBuf,
    /// Parsed frontmatter metadata
    pub metadata: SkillMetadata,
    /// Markdown body (the actual instructions)
    pub content: String,
    /// Source where this skill was loaded from
    pub source: SkillSource,
}

/// Metadata from SKILL.md YAML frontmatter
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillMetadata {
    /// Required: human-readable name
    pub name: String,

    /// Required: what this skill does
    pub description: String,

    /// Optional: semver version
    #[serde(default)]
    pub version: Option<String>,

    /// Optional: SPDX license identifier
    #[serde(default)]
    pub license: Option<String>,

    /// Optional: compatible agent identifiers
    #[serde(default)]
    pub compatibility: Option<Vec<String>>,

    /// Optional: tool access control
    /// Examples: ["*"], ["read_tool", "write_file"], ["!shell", "!delete_file"]
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<Vec<String>>,

    /// Optional: categorization tags
    #[serde(default)]
    pub tags: Option<Vec<String>>,

    /// Optional: author name/email
    #[serde(default)]
    pub author: Option<String>,

    /// Extension fields for future compatibility
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Where a skill was discovered from
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// Global paths (~/.config/querymt/skills, ~/.claude/skills, etc.)
    Global(PathBuf),
    /// Project-relative paths (.skills/, .claude/skills/, etc.)
    Project(PathBuf),
    /// Explicit config path
    Configured(PathBuf),
    /// Remote URL with caching
    Remote { url: String, cached_at: PathBuf },
}

impl SkillSource {
    /// Priority for deduplication (higher = overrides lower)
    pub fn priority(&self) -> u8 {
        match self {
            SkillSource::Global(_) => 1,
            SkillSource::Project(_) => 2,
            SkillSource::Configured(_) => 3,
            SkillSource::Remote { .. } => 4,
        }
    }
}

/// Parsed tool policy from `allowed-tools` field
#[derive(Debug, Clone, Default)]
pub enum ToolAccessPolicy {
    /// No restrictions (default, or `["*"]`)
    #[default]
    All,

    /// Block all tools
    None,

    /// Only allow these specific tools
    Whitelist(Vec<String>),

    /// Allow all EXCEPT these tools (parsed from `["!tool1", "!tool2"]`)
    Blacklist(Vec<String>),
}

impl SkillMetadata {
    /// Parse allowed_tools into a ToolAccessPolicy
    pub fn tool_policy(&self) -> ToolAccessPolicy {
        match &self.allowed_tools {
            None => ToolAccessPolicy::All,
            Some(tools) if tools.is_empty() => ToolAccessPolicy::None,
            Some(tools) if tools.iter().any(|t| t == "*") => ToolAccessPolicy::All,
            Some(tools) => {
                let has_blacklist = tools.iter().any(|t| t.starts_with('!'));
                let has_whitelist = tools.iter().any(|t| !t.starts_with('!'));

                if has_blacklist && has_whitelist {
                    log::warn!(
                        "Skill '{}' mixes whitelist and blacklist syntax, using whitelist only",
                        self.name
                    );
                }

                if has_blacklist {
                    let blacklist = tools
                        .iter()
                        .filter_map(|t| t.strip_prefix('!'))
                        .map(|s| s.to_string())
                        .collect();
                    ToolAccessPolicy::Blacklist(blacklist)
                } else {
                    ToolAccessPolicy::Whitelist(tools.clone())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_policy_all() {
        let meta = SkillMetadata {
            name: "test".into(),
            description: "test".into(),
            version: None,
            license: None,
            compatibility: None,
            allowed_tools: None,
            tags: None,
            author: None,
            extra: HashMap::new(),
        };
        assert!(matches!(meta.tool_policy(), ToolAccessPolicy::All));
    }

    #[test]
    fn test_tool_policy_whitelist() {
        let meta = SkillMetadata {
            name: "test".into(),
            description: "test".into(),
            version: None,
            license: None,
            compatibility: None,
            allowed_tools: Some(vec!["read_tool".into(), "write_file".into()]),
            tags: None,
            author: None,
            extra: HashMap::new(),
        };
        if let ToolAccessPolicy::Whitelist(tools) = meta.tool_policy() {
            assert_eq!(tools.len(), 2);
            assert!(tools.contains(&"read_tool".to_string()));
        } else {
            panic!("Expected whitelist");
        }
    }

    #[test]
    fn test_tool_policy_blacklist() {
        let meta = SkillMetadata {
            name: "test".into(),
            description: "test".into(),
            version: None,
            license: None,
            compatibility: None,
            allowed_tools: Some(vec!["!shell".into(), "!delete_file".into()]),
            tags: None,
            author: None,
            extra: HashMap::new(),
        };
        if let ToolAccessPolicy::Blacklist(tools) = meta.tool_policy() {
            assert_eq!(tools.len(), 2);
            assert!(tools.contains(&"shell".to_string()));
        } else {
            panic!("Expected blacklist");
        }
    }

    #[test]
    fn test_source_priority() {
        let global = SkillSource::Global(PathBuf::from("/global"));
        let project = SkillSource::Project(PathBuf::from("/project"));
        let configured = SkillSource::Configured(PathBuf::from("/config"));

        assert!(project.priority() > global.priority());
        assert!(configured.priority() > project.priority());
    }
}
