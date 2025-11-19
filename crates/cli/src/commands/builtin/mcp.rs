use crate::commands::{CommandResult, SlashCommand};
use crate::mcp_registry;
use anyhow::Result;
use async_trait::async_trait;

/// MCP registry command - handles /mcp subcommands
pub struct McpCommand;

#[async_trait]
impl SlashCommand for McpCommand {
    fn name(&self) -> &str {
        "mcp"
    }

    fn description(&self) -> &str {
        "Interact with MCP registry (list, search, info, add)"
    }

    fn usage(&self) -> &str {
        "list [--no-cache] [--limit N] | search <query> | info <server-id> [version] | add <server-id> [version]"
    }

    fn subcommands(&self) -> Vec<(&str, &str)> {
        vec![
            ("list", "List MCP servers from registry"),
            ("search", "Search MCP registry"),
            ("info", "Show detailed server information"),
            ("add", "Add server to local config"),
        ]
    }

    fn is_async(&self) -> bool {
        true
    }

    async fn execute_async(&self, args: Vec<String>) -> Result<CommandResult> {
        if args.is_empty() {
            return Ok(CommandResult::Error(
                "Usage: /mcp list|search|info|add [args]\nTry '/help mcp' for more details".to_string()
            ));
        }

        let subcommand = &args[0];
        let remaining_args = &args[1..];

        match subcommand.as_str() {
            "list" => self.handle_list(remaining_args).await,
            "search" => self.handle_search(remaining_args).await,
            "info" => self.handle_info(remaining_args).await,
            "add" => self.handle_add(remaining_args).await,
            _ => Ok(CommandResult::Error(
                format!("Unknown subcommand: {}. Available: list, search, info, add", subcommand)
            )),
        }
    }
}

impl McpCommand {
    async fn handle_list(&self, args: &[String]) -> Result<CommandResult> {
        let mut no_cache = false;
        let mut refresh = false;
        let mut registry_url = None;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--no-cache" => no_cache = true,
                "--refresh" => refresh = true,
                "--registry" => {
                    if i + 1 < args.len() {
                        registry_url = Some(args[i + 1].clone());
                        i += 1;
                    } else {
                        return Ok(CommandResult::Error("--registry requires a URL".to_string()));
                    }
                }
                _ => {
                    return Ok(CommandResult::Error(format!("Unknown option: {}", args[i])));
                }
            }
            i += 1;
        }

        // Execute the actual list operation
        match mcp_registry::handle_list(registry_url, no_cache, refresh).await {
            Ok(_) => Ok(CommandResult::Success(String::new())),
            Err(e) => Ok(CommandResult::Error(format!("Failed to list servers: {}", e))),
        }
    }

    async fn handle_search(&self, args: &[String]) -> Result<CommandResult> {
        if args.is_empty() {
            return Ok(CommandResult::Error("Usage: /mcp search <query>".to_string()));
        }

        let query = args.join(" ");

        match mcp_registry::handle_search(query, None, false).await {
            Ok(_) => Ok(CommandResult::Success(String::new())),
            Err(e) => Ok(CommandResult::Error(format!("Failed to search: {}", e))),
        }
    }

    async fn handle_info(&self, args: &[String]) -> Result<CommandResult> {
        if args.is_empty() {
            return Ok(CommandResult::Error("Usage: /mcp info <server-id> [version]".to_string()));
        }

        let server_id = args[0].clone();
        let version = args.get(1).cloned();

        match mcp_registry::handle_info(server_id, version, None, false).await {
            Ok(_) => Ok(CommandResult::Success(String::new())),
            Err(e) => Ok(CommandResult::Error(format!("Failed to get server info: {}", e))),
        }
    }

    async fn handle_add(&self, args: &[String]) -> Result<CommandResult> {
        if args.is_empty() {
            return Ok(CommandResult::Error("Usage: /mcp add <server-id> [version]".to_string()));
        }

        let server_id = &args[0];
        let version = args.get(1).map(|s| s.as_str()).unwrap_or("latest");

        // TODO: Implement adding server to local config
        // For now, return a helpful message
        Ok(CommandResult::Success(
            format!("To add {} (version: {}) to your config:\n\n\
                    1. Edit your MCP config file (e.g., ~/.qmt/mcp-config.toml)\n\
                    2. Add the following section:\n\n\
                    [[mcp]]\n\
                    name = \"your-name\"\n\
                    source = \"registry\"\n\
                    registry_id = \"{}\"\n\
                    version = \"{}\"\n\n\
                    Full implementation coming soon!",
                    server_id, version, server_id, version)
        ))
    }
}
