use super::*;

// ============================================================================
// Skills Configuration
// ============================================================================

fn default_agent_id() -> String {
    "querymt".to_string()
}

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

    /// Agent identifier for compatibility filtering
    #[serde(default = "default_agent_id")]
    pub agent_id: String,

    /// Skill permissions
    #[serde(default)]
    pub permissions: crate::skills::SkillPermissions,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_external: true,
            paths: vec![],
            urls: vec![],
            agent_id: default_agent_id(),
            permissions: crate::skills::SkillPermissions::default(),
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
}

impl Default for SlashCommandsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            include_global: true,
            include_project: true,
            paths: vec![],
            scripts: crate::slash_commands::SlashCommandScriptsConfig::default(),
        }
    }
}
