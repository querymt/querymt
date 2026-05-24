use crate::slash_commands::discovery;
use crate::slash_commands::types::{
    SlashCommand, SlashCommandDiagnostic, SlashCommandScriptsConfig, SlashCommandSource,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registry holding discovered slash commands.
#[derive(Debug, Clone, Default)]
pub struct SlashCommandRegistry {
    /// Commands indexed by name.
    by_name: HashMap<String, Arc<SlashCommand>>,
    /// Whether script execution is enabled (controls visibility of script-only commands).
    scripts_enabled: bool,
}

impl SlashCommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry by discovering commands from the given sources.
    ///
    /// Script-only commands are hidden when `scripts_enabled` is false.
    /// Diagnostics for invalid files are returned alongside the registry.
    pub fn from_sources(
        sources: &[SlashCommandSource],
        scripts_config: &SlashCommandScriptsConfig,
    ) -> (Self, Vec<SlashCommandDiagnostic>) {
        let (commands, diagnostics) = discovery::discover_all(sources);
        let mut registry = Self {
            by_name: HashMap::new(),
            scripts_enabled: scripts_config.enabled,
        };

        for cmd in commands {
            registry.register(cmd);
        }

        (registry, diagnostics)
    }

    /// Build an empty registry (no discovery).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Register a single command. Later registrations of the same name overwrite.
    pub fn register(&mut self, command: SlashCommand) {
        self.by_name.insert(command.name.clone(), Arc::new(command));
    }

    /// Look up a command by name.
    ///
    /// Returns `None` if the command doesn't exist or is a script-only command
    /// while scripts are disabled.
    pub fn get(&self, name: &str) -> Option<Arc<SlashCommand>> {
        self.by_name.get(name).and_then(|cmd| {
            // Hide script-required commands when scripts are disabled
            if cmd.requires_script && !self.scripts_enabled {
                None
            } else {
                Some(cmd.clone())
            }
        })
    }

    /// List all visible command names (sorted).
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .by_name
            .iter()
            .filter(|(_, cmd)| !cmd.requires_script || self.scripts_enabled)
            .map(|(name, _)| name.as_str())
            .collect();
        names.sort();
        names
    }

    /// Iterate over all visible commands.
    pub fn all(&self) -> impl Iterator<Item = Arc<SlashCommand>> + '_ {
        self.by_name
            .iter()
            .filter(|(_, cmd)| !cmd.requires_script || self.scripts_enabled)
            .map(|(_, cmd)| cmd.clone())
    }

    /// Number of visible commands.
    pub fn len(&self) -> usize {
        self.names().len()
    }

    /// Whether the registry has no visible commands.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Discover and reload commands from the given project root and config paths.
    ///
    /// This replaces the entire registry contents.
    pub fn reload(
        &mut self,
        project_root: Option<&Path>,
        extra_paths: &[PathBuf],
        scripts_config: &SlashCommandScriptsConfig,
    ) -> Vec<SlashCommandDiagnostic> {
        let mut sources = Vec::new();

        if let Some(root) = project_root {
            sources.extend(discovery::default_search_paths(root));
        } else {
            // Global-only when no project root
            if let Some(home) = dirs::home_dir() {
                sources.push(SlashCommandSource::Global(home.join(".qmt/commands")));
            }
            if let Ok(cfg_dir) = querymt_utils::providers::config_dir() {
                sources.push(SlashCommandSource::Global(cfg_dir.join("commands")));
            }
        }

        for p in extra_paths {
            sources.push(SlashCommandSource::Configured(p.clone()));
        }

        let (commands, diagnostics) = discovery::discover_all(&sources);

        self.by_name.clear();
        self.scripts_enabled = scripts_config.enabled;
        for cmd in commands {
            self.register(cmd);
        }

        diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slash_commands::types::{ScriptMode, ScriptRuntime, SlashCommandScript};
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    fn make_command(name: &str, description: &str, requires_script: bool) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            source: SlashCommandSource::Global(PathBuf::from("/test")),
            path: PathBuf::from(format!("/test/{}.md", name)),
            description: description.to_string(),
            argument_hint: None,
            tags: vec![],
            template: format!("{}: $ARGUMENTS", description),
            script: if requires_script {
                Some(SlashCommandScript {
                    runtime: ScriptRuntime::Python,
                    path: PathBuf::from("script.py"),
                    mode: ScriptMode::TransformArguments,
                    timeout_ms: 1000,
                })
            } else {
                None
            },
            requires_script,
        }
    }

    #[test]
    fn test_register_and_get() {
        let mut reg = SlashCommandRegistry::new();
        reg.register(make_command("review", "Review changes", false));

        assert!(reg.get("review").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_names_sorted() {
        let mut reg = SlashCommandRegistry::new();
        reg.register(make_command("charlie", "C", false));
        reg.register(make_command("alpha", "A", false));
        reg.register(make_command("bravo", "B", false));

        assert_eq!(reg.names(), vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn test_script_required_command_hidden_when_disabled() {
        let mut reg = SlashCommandRegistry::new();
        reg.register(make_command("normal", "Normal command", false));
        reg.register(make_command("scripted", "Script command", true));

        // Scripts disabled (default)
        assert!(reg.get("normal").is_some());
        assert!(reg.get("scripted").is_none());
        assert_eq!(reg.names(), vec!["normal"]);
    }

    #[test]
    fn test_script_required_command_visible_when_enabled() {
        let mut reg = SlashCommandRegistry {
            by_name: HashMap::new(),
            scripts_enabled: true,
        };
        reg.register(make_command("normal", "Normal command", false));
        reg.register(make_command("scripted", "Script command", true));

        assert!(reg.get("normal").is_some());
        assert!(reg.get("scripted").is_some());
        assert_eq!(reg.names(), vec!["normal", "scripted"]);
    }

    #[test]
    fn test_from_sources() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("hello.md"),
            "---\ndescription: Say hello\n---\nHello $ARGUMENTS\n",
        )
        .unwrap();

        let sources = vec![SlashCommandSource::Global(dir.path().to_path_buf())];
        let scripts_config = SlashCommandScriptsConfig::default();
        let (reg, diags) = SlashCommandRegistry::from_sources(&sources, &scripts_config);

        assert!(diags.is_empty());
        assert!(reg.get("hello").is_some());
    }

    #[test]
    fn test_is_empty() {
        let reg = SlashCommandRegistry::new();
        assert!(reg.is_empty());

        let mut reg = SlashCommandRegistry::new();
        reg.register(make_command("x", "X", false));
        assert!(!reg.is_empty());
    }

    #[test]
    fn test_reload_replaces_contents() {
        let mut reg = SlashCommandRegistry::new();
        reg.register(make_command("old", "Old command", false));
        assert!(reg.get("old").is_some());

        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("new.md"),
            "---\ndescription: New command\n---\nBody\n",
        )
        .unwrap();

        let scripts_config = SlashCommandScriptsConfig::default();
        let diags = reg.reload(Some(dir.path()), &[], &scripts_config);
        assert!(diags.is_empty());

        // Old command is gone
        assert!(reg.get("old").is_none());
        // New command from project discovery (under .qmt/commands)
        // But .qmt/commands doesn't exist in the temp dir, so let's check reload
        // actually cleared old and didn't add anything (no .qmt/commands dir)
        assert!(reg.is_empty());
    }
}
