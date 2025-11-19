use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub mod builtin;
pub mod completer;
pub mod markdown;

use markdown::MarkdownCommand;

/// Result of executing a slash command
#[derive(Debug, Clone)]
pub enum CommandResult {
    /// Command succeeded with output to display
    Success(String),
    /// Command failed with error message
    Error(String),
    /// Command executed but should continue normal chat flow
    Continue,
    /// Command requests to exit the application
    Exit,
}

/// Trait for implementing slash commands
#[async_trait]
pub trait SlashCommand: Send + Sync {
    /// The command name (without the leading /)
    fn name(&self) -> &str;

    /// Short description of what the command does
    fn description(&self) -> &str;

    /// Usage hint showing expected arguments
    /// Example: "list [--no-cache] [--limit N]"
    fn usage(&self) -> &str;

    /// Execute the command with the given arguments (synchronous version)
    /// Override this for simple commands that don't need async
    fn execute(&self, _args: Vec<String>) -> Result<CommandResult> {
        Err(anyhow::anyhow!("Command requires async execution. Use execute_async instead."))
    }

    /// Execute the command with the given arguments (async version)
    /// Override this for commands that need async operations
    async fn execute_async(&self, args: Vec<String>) -> Result<CommandResult> {
        // Default: fall back to sync execute
        self.execute(args)
    }

    /// Check if this command requires async execution
    fn is_async(&self) -> bool {
        false
    }

    /// Get available subcommands for tab completion
    /// Returns a list of (subcommand_name, description) tuples
    fn subcommands(&self) -> Vec<(&str, &str)> {
        Vec::new()
    }
}

/// Source of a command - either built-in Rust or loaded from markdown
#[derive(Clone)]
pub enum CommandSource {
    BuiltIn(Arc<dyn SlashCommand>),
    Markdown(MarkdownCommand),
}

impl CommandSource {
    pub fn name(&self) -> &str {
        match self {
            CommandSource::BuiltIn(cmd) => cmd.name(),
            CommandSource::Markdown(cmd) => cmd.name(),
        }
    }

    pub fn description(&self) -> &str {
        match self {
            CommandSource::BuiltIn(cmd) => cmd.description(),
            CommandSource::Markdown(cmd) => cmd.description(),
        }
    }

    pub fn usage(&self) -> &str {
        match self {
            CommandSource::BuiltIn(cmd) => cmd.usage(),
            CommandSource::Markdown(cmd) => cmd.usage(),
        }
    }

    pub fn subcommands(&self) -> Vec<(&str, &str)> {
        match self {
            CommandSource::BuiltIn(cmd) => cmd.subcommands(),
            CommandSource::Markdown(_) => Vec::new(),
        }
    }

    pub fn is_async(&self) -> bool {
        match self {
            CommandSource::BuiltIn(cmd) => cmd.is_async(),
            CommandSource::Markdown(_) => false, // Markdown commands use LLM which is already async
        }
    }
}

/// Registry that manages all available slash commands
pub struct CommandRegistry {
    commands: HashMap<String, CommandSource>,
}

impl CommandRegistry {
    /// Create a new command registry
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
        }
    }

    /// Register a built-in command
    pub fn register_builtin(&mut self, command: Arc<dyn SlashCommand>) {
        let name = command.name().to_string();
        self.commands.insert(name, CommandSource::BuiltIn(command));
    }

    /// Register a markdown command
    pub fn register_markdown(&mut self, command: MarkdownCommand) {
        let name = command.name().to_string();
        self.commands.insert(name, CommandSource::Markdown(command));
    }

    /// Get a command by name
    pub fn get(&self, name: &str) -> Option<&CommandSource> {
        self.commands.get(name)
    }

    /// List all available commands
    pub fn list_commands(&self) -> Vec<&CommandSource> {
        self.commands.values().collect()
    }

    /// Parse a command line into command name and arguments
    pub fn parse_command_line(input: &str) -> Option<(&str, Vec<String>)> {
        let input = input.trim();
        if !input.starts_with('/') {
            return None;
        }

        let without_slash = &input[1..];
        let parts: Vec<&str> = without_slash.split_whitespace().collect();

        if parts.is_empty() {
            return None;
        }

        let cmd_name = parts[0];
        let args = parts[1..].iter().map(|s| s.to_string()).collect();

        Some((cmd_name, args))
    }

    /// Execute a built-in command
    pub fn execute_builtin(&self, name: &str, args: Vec<String>) -> Result<CommandResult> {
        match self.commands.get(name) {
            Some(CommandSource::BuiltIn(cmd)) => cmd.execute(args),
            Some(CommandSource::Markdown(_)) => {
                Err(anyhow::anyhow!("Command '{}' is a markdown command and requires async execution", name))
            }
            None => Err(anyhow::anyhow!("Unknown command: /{}", name)),
        }
    }

    /// Get all command names for completion
    pub fn command_names(&self) -> Vec<String> {
        self.commands.keys().map(|k| k.clone()).collect()
    }
}

/// Loads commands from markdown files
pub struct CommandLoader {
    user_commands_dir: PathBuf,
    project_commands_dir: PathBuf,
}

impl CommandLoader {
    /// Create a new command loader
    pub fn new() -> Result<Self> {
        let home_dir = dirs::home_dir()
            .context("Could not determine home directory")?;

        let user_commands_dir = home_dir.join(".qmt").join("commands");
        let project_commands_dir = PathBuf::from(".qmt/commands");

        Ok(Self {
            user_commands_dir,
            project_commands_dir,
        })
    }

    /// Discover and load all markdown commands
    pub fn load_commands(&self) -> Result<Vec<MarkdownCommand>> {
        let mut commands = Vec::new();

        // Load user commands first
        if self.user_commands_dir.exists() {
            commands.extend(self.load_from_directory(&self.user_commands_dir)?);
        }

        // Load project commands (these override user commands)
        if self.project_commands_dir.exists() {
            commands.extend(self.load_from_directory(&self.project_commands_dir)?);
        }

        Ok(commands)
    }

    fn load_from_directory(&self, dir: &PathBuf) -> Result<Vec<MarkdownCommand>> {
        let mut commands = Vec::new();

        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("Failed to read directory: {:?}", dir))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                match MarkdownCommand::from_file(path.clone()) {
                    Ok(cmd) => commands.push(cmd),
                    Err(e) => {
                        log::warn!("Failed to load command from {:?}: {}", path, e);
                    }
                }
            }
        }

        Ok(commands)
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for CommandLoader {
    fn default() -> Self {
        Self::new().expect("Failed to create command loader")
    }
}
