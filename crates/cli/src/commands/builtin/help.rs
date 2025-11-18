use crate::commands::{CommandResult, SlashCommand};
use anyhow::Result;
use async_trait::async_trait;
use colored::Colorize;

/// Help command - shows available commands
/// Note: This shows a static list of built-in commands.
/// Markdown commands are automatically discoverable via tab completion.
pub struct HelpCommand;

#[async_trait]
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "Show available slash commands"
    }

    fn usage(&self) -> &str {
        "[command]"
    }

    fn execute(&self, args: Vec<String>) -> Result<CommandResult> {
        if args.is_empty() {
            // Show all built-in commands
            let mut output = String::new();
            output.push_str(&format!("{}\n\n", "Built-in Commands:".bold()));

            let builtin_commands = vec![
                ("/help [command]", "Show available slash commands"),
                ("/mcp list|search|info|add", "Interact with MCP registry"),
                ("  Options: --no-cache, --refresh", ""),
                ("/clear", "Clear the screen"),
                ("/exit", "Exit the application"),
            ];

            for (cmd, desc) in builtin_commands {
                output.push_str(&format!("  {}\n", cmd.cyan()));
                output.push_str(&format!("    {}\n\n", desc));
            }

            output.push_str(&format!("{}\n", "Custom commands loaded from ~/.qmt/commands/ and .qmt/commands/ are also available.".dimmed()));
            output.push_str(&format!("{}\n", "Use Tab for command completion with fuzzy search.".dimmed()));

            Ok(CommandResult::Success(output))
        } else {
            // Show help for specific built-in command
            let cmd_name = &args[0];

            let help_text = match cmd_name.as_str() {
                "mcp" => {
                    format!(
                        "{}\n\n{}\n\n{}\n  {} /mcp list [--no-cache | --refresh]\n  {} /mcp search <query>\n  {} /mcp info <server-id> [version]\n  {} /mcp add <server-id> [version]\n\n{}\n  {} --no-cache: Skip cache, fetch fresh data with pagination\n  {} --refresh: Fetch ALL servers and update cache",
                        "/mcp".cyan().bold(),
                        "Interact with the Model Context Protocol (MCP) registry",
                        "Usage:".bold(),
                        "•".dimmed(),
                        "•".dimmed(),
                        "•".dimmed(),
                        "•".dimmed(),
                        "Flags:".bold(),
                        "•".dimmed(),
                        "•".dimmed()
                    )
                }
                "clear" => format!("{}\n\n{}", "/clear".cyan().bold(), "Clear the screen"),
                "exit" => format!("{}\n\n{}", "/exit".cyan().bold(), "Exit the application"),
                "help" => format!("{}\n\n{}\n\n{} /help [command]", "/help".cyan().bold(), "Show available slash commands or detailed help for a specific command", "Usage:".bold()),
                _ => {
                    return Ok(CommandResult::Error(format!(
                        "Unknown command: /{}. Type '/help' to see available commands.",
                        cmd_name
                    )))
                }
            };

            Ok(CommandResult::Success(help_text))
        }
    }
}
