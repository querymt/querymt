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

/// What went wrong with one skill folder entry during discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillProblemKind {
    /// A skill source folder exists but cannot be read.
    SourceUnreadable,
    /// An entry inside a readable source cannot be read.
    EntryUnreadable,
    /// A symbolic link whose target does not exist.
    BrokenLink,
    /// A symbolic link that points back into its own ancestry.
    LinkLoop,
    /// One entry directory holds both `skill.md` and `SKILL.md`.
    DuplicateDefinition,
    /// A skill definition file cannot be parsed or fails validation.
    InvalidDefinition,
    /// Two skills of equal precedence define the same callable ID.
    DuplicateId,
}

impl SkillProblemKind {
    /// Stable snake_case identifier used on the wire and in diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceUnreadable => "source_unreadable",
            Self::EntryUnreadable => "entry_unreadable",
            Self::BrokenLink => "broken_link",
            Self::LinkLoop => "link_loop",
            Self::DuplicateDefinition => "duplicate_definition",
            Self::InvalidDefinition => "invalid_definition",
            Self::DuplicateId => "duplicate_id",
        }
    }
}

impl std::fmt::Display for SkillProblemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A skill that was found but cannot be offered, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillProblem {
    /// Affected path (definition file, entry directory or link).
    pub path: PathBuf,
    /// The skill source folder the path belongs to.
    pub source: PathBuf,
    pub kind: SkillProblemKind,
    /// Human-readable explanation.
    pub message: String,
}

/// The kind of a skill source, without its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSourceKind {
    Global,
    Project,
    Configured,
    Remote,
}

impl SkillSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Configured => "configured",
            Self::Remote => "remote",
        }
    }
}

impl From<&SkillSource> for SkillSourceKind {
    fn from(source: &SkillSource) -> Self {
        match source {
            SkillSource::Global(_) => Self::Global,
            SkillSource::Project(_) => Self::Project,
            SkillSource::Configured(_) => Self::Configured,
            SkillSource::Remote { .. } => Self::Remote,
        }
    }
}

/// One offered local skill with its origin; the body is not part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInventoryEntry {
    /// Callable ID.
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_kind: SkillSourceKind,
    /// The skill source folder the skill was found in.
    pub source_folder: PathBuf,
    /// The skill directory, as a path under the source folder.
    pub directory: PathBuf,
    /// The resolved directory when the source folder itself, or a path
    /// component between it and the skill directory, is a symbolic link.
    pub link_target: Option<PathBuf>,
}

impl SkillInventoryEntry {
    /// Build the entry of an offered skill.
    pub fn from_skill(skill: &Skill) -> Self {
        let source_folder = crate::skills::discovery::source_base_path(&skill.source).to_path_buf();
        let link_target = resolved_link_target(&source_folder, &skill.path);
        Self {
            id: skill.metadata.effective_id().to_string(),
            name: skill.metadata.name.clone(),
            description: skill.metadata.description.clone(),
            source_kind: SkillSourceKind::from(&skill.source),
            source_folder,
            directory: skill.path.clone(),
            link_target,
        }
    }
}

/// The canonical directory of `directory` when a path component between
/// `source_folder` and `directory` (inclusive) is a symbolic link.
fn resolved_link_target(
    source_folder: &std::path::Path,
    directory: &std::path::Path,
) -> Option<PathBuf> {
    let is_link = |path: &std::path::Path| {
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
    };
    let relative = directory.strip_prefix(source_folder).ok()?;
    // The source folder counts too; only its own final component is checked,
    // so a linked parent such as `/var` -> `/private/var` is not a link here.
    let mut current = source_folder.to_path_buf();
    let mut through_link = is_link(&current);
    if !through_link {
        for component in relative.components() {
            current.push(component);
            if is_link(&current) {
                through_link = true;
                break;
            }
        }
    }
    if through_link {
        directory.canonicalize().ok()
    } else {
        None
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
