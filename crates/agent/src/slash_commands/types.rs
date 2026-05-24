use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// A loaded slash command definition.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// Command name derived from filename (without `.md`).
    pub name: String,
    /// Where this command was discovered from.
    pub source: SlashCommandSource,
    /// Absolute path to the `.md` file.
    pub path: PathBuf,
    /// Required human-readable description used for ACP advertising.
    pub description: String,
    /// Optional hint shown to the user for the arguments slot.
    pub argument_hint: Option<String>,
    /// Arbitrary tags for filtering/display.
    pub tags: Vec<String>,
    /// The prompt template body (markdown).
    pub template: String,
    /// Optional script definition (not executed unless scripts are enabled).
    pub script: Option<SlashCommandScript>,
    /// Whether this command requires a script to function.
    /// If true and scripts are disabled, the command is hidden from ACP.
    pub requires_script: bool,
}

/// Where a command was discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandSource {
    /// `~/.qmt/commands`
    Global(PathBuf),
    /// `<PROJECT_ROOT>/.qmt/commands`
    Project(PathBuf),
    /// Explicit config path.
    Configured(PathBuf),
}

impl SlashCommandSource {
    /// Priority for deduplication (higher = overrides lower).
    pub fn priority(&self) -> u8 {
        match self {
            SlashCommandSource::Global(_) => 1,
            SlashCommandSource::Project(_) => 2,
            SlashCommandSource::Configured(_) => 3,
        }
    }
}

/// Parsed result of a slash command invocation from user text.
#[derive(Debug, Clone)]
pub struct SlashCommandInvocation {
    /// The matched command name.
    pub name: String,
    /// Everything after the command name (may be empty).
    pub arguments: String,
    /// The original text that matched.
    pub original_text: String,
}

/// The expanded prompt produced by a slash command.
#[derive(Debug, Clone)]
pub struct SlashCommandExpansion {
    /// The invocation that triggered this expansion.
    pub invocation: SlashCommandInvocation,
    /// The rendered prompt text to inject.
    pub prompt_text: String,
}

/// Script definition parsed from command frontmatter.
///
/// Scripts are not executed in the initial implementation.
/// This struct captures the metadata so commands can be validated
/// and hidden when scripts are disabled.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SlashCommandScript {
    /// Runtime to use: `python` or `javascript`.
    pub runtime: ScriptRuntime,
    /// Path to the script, relative to the command file's directory.
    pub path: PathBuf,
    /// How the script output is used.
    #[serde(default)]
    pub mode: ScriptMode,
    /// Maximum execution time in milliseconds.
    #[serde(default = "default_script_timeout_ms")]
    pub timeout_ms: u64,
}

/// Supported script runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptRuntime {
    Python,
    JavaScript,
}

/// How the script output integrates into the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScriptMode {
    /// Script returns variables; markdown template still renders final prompt.
    #[default]
    TransformArguments,
    /// Script returns the full prompt text; markdown body is fallback.
    GeneratePrompt,
}

/// Configuration for script execution (all off by default).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SlashCommandScriptsConfig {
    /// Enable script-backed commands.
    #[serde(default)]
    pub enabled: bool,

    /// Allow Python scripts.
    #[serde(default = "crate::slash_commands::types::default_true_val")]
    pub allow_python: bool,

    /// Allow JavaScript scripts.
    #[serde(default = "crate::slash_commands::types::default_true_val")]
    pub allow_javascript: bool,

    /// Default timeout for script execution.
    #[serde(default = "default_script_timeout_ms")]
    pub timeout_ms: u64,
}

/// A diagnostic produced during discovery (invalid file, missing field, etc.).
#[derive(Debug, Clone)]
pub struct SlashCommandDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

fn default_script_timeout_ms() -> u64 {
    1000
}

fn default_true_val() -> bool {
    true
}

/// Metadata parsed from YAML frontmatter of a command `.md` file.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandFrontmatter {
    /// Required: human-readable description.
    pub description: String,

    /// Optional: hint for the arguments slot.
    #[serde(default, rename = "argument-hint")]
    pub argument_hint: Option<String>,

    /// Optional: tags.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Optional: script definition.
    pub script: Option<SlashCommandScript>,

    /// Whether this command requires its script to function.
    /// When `true` and scripts are disabled, the command is hidden.
    #[serde(default, rename = "requires-script")]
    pub requires_script: bool,

    /// Extension fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Validates that a command name is syntactically valid.
///
/// Valid names: `^[a-zA-Z][a-zA-Z0-9_-]*$`
pub fn is_valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_names() {
        assert!(is_valid_command_name("plan"));
        assert!(is_valid_command_name("my-command"));
        assert!(is_valid_command_name("my_command"));
        assert!(is_valid_command_name("Command123"));
    }

    #[test]
    fn test_invalid_names() {
        assert!(!is_valid_command_name(""));
        assert!(!is_valid_command_name("123abc"));
        assert!(!is_valid_command_name("has space"));
        assert!(!is_valid_command_name("has/slash"));
        assert!(!is_valid_command_name(".hidden"));
        assert!(!is_valid_command_name("-leading-dash"));
    }

    #[test]
    fn test_source_priority() {
        let global = SlashCommandSource::Global(PathBuf::from("/global"));
        let project = SlashCommandSource::Project(PathBuf::from("/project"));
        let configured = SlashCommandSource::Configured(PathBuf::from("/config"));

        assert!(project.priority() > global.priority());
        assert!(configured.priority() > project.priority());
    }
}
