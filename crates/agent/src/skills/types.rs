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
    ///
    /// Deserialized with a default so protocol entries can omit it; the
    /// parser fills protocol defaults before validation reports it.
    #[serde(default)]
    pub name: String,

    /// Required: what this skill does
    pub description: String,

    /// Optional: `.agents` protocol stable skill ID.
    /// Falls back to the entry directory name when absent.
    #[serde(default)]
    pub id: Option<String>,

    /// Optional: `.agents` protocol enabled flag. Absent means enabled.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Optional: semver version
    #[serde(default)]
    pub version: Option<String>,

    /// Optional: SPDX license identifier
    #[serde(default)]
    pub license: Option<String>,

    /// Optional: environment requirements for using the skill
    #[serde(default)]
    pub compatibility: Option<String>,

    /// Optional: space-separated list of pre-approved tools
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<String>,

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
    /// Effective stable ID: the explicit protocol `id`, else the skill name
    /// (which defaults to the entry directory name for protocol skills).
    pub fn effective_id(&self) -> &str {
        self.id.as_deref().unwrap_or(&self.name)
    }

    /// `.agents` protocol enabled state; absent means enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Parse the specification's space-separated `allowed-tools` field.
    pub fn tool_policy(&self) -> ToolAccessPolicy {
        let Some(allowed_tools) = self.allowed_tools.as_deref() else {
            return ToolAccessPolicy::All;
        };
        let tools: Vec<String> = allowed_tools
            .split_whitespace()
            .map(str::to_string)
            .collect();

        if tools.is_empty() {
            return ToolAccessPolicy::None;
        }
        if tools.iter().any(|tool| tool == "*") {
            return ToolAccessPolicy::All;
        }

        let has_blacklist = tools.iter().any(|tool| tool.starts_with('!'));
        let has_whitelist = tools.iter().any(|tool| !tool.starts_with('!'));
        if has_blacklist && has_whitelist {
            log::warn!(
                "Skill '{}' mixes whitelist and blacklist syntax, using whitelist only",
                self.name
            );
        }

        if has_blacklist {
            ToolAccessPolicy::Blacklist(
                tools
                    .iter()
                    .filter_map(|tool| tool.strip_prefix('!'))
                    .map(str::to_string)
                    .collect(),
            )
        } else {
            ToolAccessPolicy::Whitelist(tools)
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
            id: None,
            enabled: None,
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
            id: None,
            enabled: None,
            version: None,
            license: None,
            compatibility: None,
            allowed_tools: Some("read_tool write_file".into()),
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
            id: None,
            enabled: None,
            version: None,
            license: None,
            compatibility: None,
            allowed_tools: Some("!shell !delete_file".into()),
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
    fn test_protocol_id_and_enabled_helpers() {
        let mut meta = SkillMetadata {
            name: "Fancy Name".into(),
            description: "test".into(),
            id: None,
            enabled: None,
            version: None,
            license: None,
            compatibility: None,
            allowed_tools: None,
            tags: None,
            author: None,
            extra: HashMap::new(),
        };
        assert_eq!(meta.effective_id(), "Fancy Name");
        assert!(meta.is_enabled());

        meta.id = Some("review".into());
        assert_eq!(meta.effective_id(), "review");

        meta.enabled = Some(false);
        assert!(!meta.is_enabled());
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
