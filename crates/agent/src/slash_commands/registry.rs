use crate::slash_commands::discovery;
use crate::slash_commands::runtime::{RegisteredRuntimeCommand, RuntimeSlashCommandPlugin};
use crate::slash_commands::types::{
    SlashCommand, SlashCommandDiagnostic, SlashCommandScriptsConfig, SlashCommandSource,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registry holding discovered slash commands.
///
/// Stores two kinds of command backends:
/// - **Prompt commands** — markdown templates expanded into LLM prompts.
/// - **Runtime commands** — deterministic plugin-backed commands.
///
/// Runtime commands take precedence over prompt commands of the same name.
pub struct SlashCommandRegistry {
    prompt_commands: HashMap<String, Arc<SlashCommand>>,
    runtime_commands: HashMap<String, RegisteredRuntimeCommand>,
    scripts_enabled: bool,
}

impl Clone for SlashCommandRegistry {
    fn clone(&self) -> Self {
        Self {
            prompt_commands: self.prompt_commands.clone(),
            runtime_commands: self.runtime_commands.clone(),
            scripts_enabled: self.scripts_enabled,
        }
    }
}

impl Default for SlashCommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SlashCommandRegistry {
    pub fn new() -> Self {
        Self {
            prompt_commands: HashMap::new(),
            runtime_commands: HashMap::new(),
            scripts_enabled: false,
        }
    }

    pub fn from_sources(
        sources: &[SlashCommandSource],
        scripts_config: &SlashCommandScriptsConfig,
    ) -> (Self, Vec<SlashCommandDiagnostic>) {
        let (commands, diagnostics) = discovery::discover_all(sources);
        let mut registry = Self {
            prompt_commands: HashMap::new(),
            runtime_commands: HashMap::new(),
            scripts_enabled: scripts_config.enabled,
        };

        for cmd in commands {
            registry.register(cmd);
        }

        (registry, diagnostics)
    }

    pub fn empty() -> Self {
        Self::new()
    }

    pub fn register(&mut self, command: SlashCommand) {
        self.prompt_commands
            .insert(command.name.clone(), Arc::new(command));
    }

    /// Register a runtime plugin and all commands it advertises.
    pub fn register_plugin(&mut self, plugin: Arc<dyn RuntimeSlashCommandPlugin>) {
        for descriptor in plugin.descriptors() {
            self.runtime_commands.insert(
                descriptor.name.to_string(),
                RegisteredRuntimeCommand {
                    descriptor,
                    plugin: plugin.clone(),
                },
            );
        }
    }

    pub fn get_runtime(&self, name: &str) -> Option<&RegisteredRuntimeCommand> {
        self.runtime_commands.get(name)
    }

    pub fn get(&self, name: &str) -> Option<Arc<SlashCommand>> {
        self.prompt_commands.get(name).and_then(|cmd| {
            if cmd.requires_script && !self.scripts_enabled {
                None
            } else {
                Some(cmd.clone())
            }
        })
    }

    pub fn is_runtime(&self, name: &str) -> bool {
        self.runtime_commands.contains_key(name)
    }

    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .prompt_commands
            .iter()
            .filter(|(_, cmd)| !cmd.requires_script || self.scripts_enabled)
            .map(|(name, _)| name.as_str())
            .chain(self.runtime_commands.keys().map(|name| name.as_str()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    pub fn all(&self) -> impl Iterator<Item = Arc<SlashCommand>> + '_ {
        self.prompt_commands
            .iter()
            .filter(|(_, cmd)| !cmd.requires_script || self.scripts_enabled)
            .map(|(_, cmd)| cmd.clone())
    }

    pub fn all_runtime(&self) -> impl Iterator<Item = &RegisteredRuntimeCommand> + '_ {
        self.runtime_commands.values()
    }

    pub fn len(&self) -> usize {
        self.names().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

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

        self.prompt_commands.clear();
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

        assert!(reg.get("normal").is_some());
        assert!(reg.get("scripted").is_none());
        assert_eq!(reg.names(), vec!["normal"]);
    }

    #[test]
    fn test_script_required_command_visible_when_enabled() {
        let mut reg = SlashCommandRegistry {
            prompt_commands: HashMap::new(),
            runtime_commands: HashMap::new(),
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
        let commands_dir = dir.path().join(".qmt/commands");
        fs::create_dir_all(&commands_dir).unwrap();
        fs::write(
            commands_dir.join("new.md"),
            "---\ndescription: New command\n---\nBody\n",
        )
        .unwrap();

        let scripts_config = SlashCommandScriptsConfig::default();
        let diags = reg.reload(Some(dir.path()), &[], &scripts_config);
        assert!(diags.is_empty());

        assert!(reg.get("old").is_none());
        assert!(reg.get("new").is_some());
    }
}
