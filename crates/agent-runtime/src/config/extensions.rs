use super::*;

// ============================================================================
// Skills Configuration
// ============================================================================

/// Configuration for skills system
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    /// Enable skills system (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Check external paths (Claude Code, agents conventions)
    #[serde(default = "default_true")]
    pub include_external: bool,

    /// Custom search paths (added to defaults)
    #[serde(default)]
    pub paths: Vec<PathBuf>,

    /// Remote skill URLs (Phase 2 - not yet implemented)
    #[serde(default)]
    pub urls: Vec<String>,

    /// Skill permissions
    #[serde(default)]
    pub permissions: crate::skills::SkillPermissions,

    /// Offer skills published by connected MCP servers (default: true)
    #[serde(default = "default_true")]
    pub mcp: bool,

    /// Directory levels below a project `.agents/skills` folder that may hold
    /// a skill directory (default: 4; `1` means direct children only).
    /// Other skill sources always accept direct children only.
    #[serde(
        default = "default_skill_nested_depth",
        deserialize_with = "deserialize_skill_nested_depth"
    )]
    #[schemars(range(min = 1, max = 16))]
    pub nested_depth: u8,
}

fn default_skill_nested_depth() -> u8 {
    crate::skills::discovery::DEFAULT_NESTED_DEPTH
}

fn deserialize_skill_nested_depth<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use crate::skills::discovery::{MAX_NESTED_DEPTH, MIN_NESTED_DEPTH};
    let invalid = || {
        <D::Error as serde::de::Error>::custom(format!(
            "`nested_depth` must be an integer from {MIN_NESTED_DEPTH} to {MAX_NESTED_DEPTH}"
        ))
    };
    let value = i64::deserialize(deserializer).map_err(|_| invalid())?;
    u8::try_from(value)
        .ok()
        .filter(|depth| (MIN_NESTED_DEPTH..=MAX_NESTED_DEPTH).contains(depth))
        .ok_or_else(invalid)
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_external: true,
            paths: vec![],
            urls: vec![],
            permissions: crate::skills::SkillPermissions::default(),
            mcp: true,
            nested_depth: crate::skills::discovery::DEFAULT_NESTED_DEPTH,
        }
    }
}

// ============================================================================
// End Skills Configuration
// ============================================================================

// ============================================================================
// Slash Commands Configuration
// ============================================================================

/// Configuration for the slash commands system
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SlashCommandsConfig {
    /// Enable slash commands system (default: true)
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Search global paths (`~/.qmt/commands`)
    #[serde(default = "default_true")]
    pub include_global: bool,

    /// Search project paths (`<PROJECT_ROOT>/.qmt/commands`)
    #[serde(default = "default_true")]
    pub include_project: bool,

    /// Custom search paths (added to defaults)
    #[serde(default)]
    pub paths: Vec<PathBuf>,

    /// Script execution configuration
    #[serde(default)]
    pub scripts: crate::slash_commands::SlashCommandScriptsConfig,

    /// Replaces the global command folders (`~/.qmt/commands` and the
    /// config directory's `commands`) when `include_global` is set. Not a
    /// configuration key: it exists so tests can point the global folder at
    /// a temporary directory.
    #[serde(skip)]
    #[schemars(skip)]
    pub global_dir: Option<PathBuf>,
}

impl Default for SlashCommandsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_global: true,
            include_project: true,
            paths: vec![],
            scripts: crate::slash_commands::SlashCommandScriptsConfig::default(),
            global_dir: None,
        }
    }
}
