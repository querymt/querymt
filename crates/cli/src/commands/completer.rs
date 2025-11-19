use reedline::{Completer, Span, Suggestion};
use std::sync::Arc;

use super::CommandRegistry;

/// Reedline completer for slash commands and MCP operations
pub struct QmtCompleter {
    command_registry: Arc<CommandRegistry>,
}

impl QmtCompleter {
    pub fn new(command_registry: Arc<CommandRegistry>) -> Self {
        Self { command_registry }
    }

    /// Complete slash commands - filter by prefix match
    fn complete_command(&self, input: &str) -> Vec<Suggestion> {
        let commands = self.command_registry.list_commands();
        let input_lower = input.to_lowercase();

        commands
            .iter()
            .filter(|cmd| {
                // Filter: show all if empty, or if command starts with input
                input.is_empty() || cmd.name().to_lowercase().starts_with(&input_lower)
            })
            .map(|cmd| {
                let cmd_name = cmd.name();
                let value = cmd_name.to_string();
                let description = if !cmd.usage().is_empty() {
                    Some(cmd.usage().to_string())
                } else {
                    Some(cmd.description().to_string())
                };

                Suggestion {
                    value,
                    description,
                    style: None,
                    extra: None,
                    span: Span::default(),
                    append_whitespace: false,
                }
            })
            .collect()
    }

    /// Complete subcommands for a given command - filter by prefix match
    fn complete_subcommand(&self, cmd_name: &str, input: &str) -> Vec<Suggestion> {
        let cmd = match self.command_registry.get(cmd_name) {
            Some(cmd) => cmd,
            None => return Vec::new(),
        };

        let subcommands = cmd.subcommands();
        if subcommands.is_empty() {
            return Vec::new();
        }

        let input_lower = input.to_lowercase();

        subcommands
            .iter()
            .filter(|(subcmd_name, _)| {
                input.is_empty() || subcmd_name.to_lowercase().starts_with(&input_lower)
            })
            .map(|(subcmd_name, description)| Suggestion {
                value: subcmd_name.to_string(),
                description: Some(description.to_string()),
                style: None,
                extra: None,
                span: Span::default(),
                append_whitespace: false,
            })
            .collect()
    }

    /// Complete server IDs from MCP cache - filter by substring match
    fn complete_server_id(&self, input: &str) -> Vec<Suggestion> {
        use crate::mcp_cache::RegistryCache;

        let server_names = RegistryCache::default_cache()
            .and_then(|cache| {
                cache.get_server_names("https://registry.modelcontextprotocol.io")
            })
            .unwrap_or_default();

        if server_names.is_empty() {
            return Vec::new();
        }

        let input_lower = input.to_lowercase();

        server_names
            .iter()
            .filter(|server_name| {
                input.is_empty() || server_name.to_lowercase().contains(&input_lower)
            })
            .map(|server_name| Suggestion {
                value: server_name.clone(),
                description: None,
                style: None,
                extra: None,
                span: Span::default(),
                append_whitespace: false,
            })
            .collect()
    }
}

impl Completer for QmtCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        // Check if we're completing a slash command
        if line.trim_start().starts_with('/') {
            let trimmed = line.trim_start();
            let without_slash = &trimmed[1..];

            // Split into parts to determine what we're completing
            let parts: Vec<&str> = without_slash.split_whitespace().collect();

            if parts.is_empty() || (parts.len() == 1 && !without_slash.ends_with(' ')) {
                // Complete command name (e.g., "/mc" -> "/mcp")
                let query = parts.first().unwrap_or(&"");
                let suggestions = self.complete_command(query);

                // Calculate span for the command name (after the '/')
                let slash_pos = line.find('/').unwrap_or(0);
                let start = slash_pos + 1;
                let end = pos;

                return suggestions
                    .into_iter()
                    .map(|mut s| {
                        s.span = Span::new(start, end);
                        s
                    })
                    .collect();
            } else if parts.len() == 1 || (parts.len() == 2 && !without_slash.ends_with(' ')) {
                // Complete subcommand (e.g., "/mcp l" -> "/mcp list")
                let cmd_name = parts[0];
                let subcmd_query = if parts.len() == 2 { parts[1] } else { "" };
                let suggestions = self.complete_subcommand(cmd_name, subcmd_query);

                if !suggestions.is_empty() {
                    // Calculate span for the subcommand
                    let cmd_end_in_line = line.find(cmd_name).unwrap_or(0) + cmd_name.len();
                    let start = if parts.len() == 2 {
                        // We're completing a partial subcommand
                        line.find(parts[1]).unwrap_or(cmd_end_in_line + 1)
                    } else {
                        // No subcommand yet, start from after the space
                        pos
                    };
                    let end = pos;

                    return suggestions
                        .into_iter()
                        .map(|mut s| {
                            s.span = Span::new(start, end);
                            s
                        })
                        .collect();
                }
            } else if parts.len() >= 2 {
                // Complete arguments (e.g., server IDs for /mcp add)
                let cmd_name = parts[0];
                let subcommand = parts[1];

                if cmd_name == "mcp" && (subcommand == "add" || subcommand == "info") && parts.len() <= 3 {
                    let server_query = if parts.len() == 3 { parts[2] } else { "" };
                    let suggestions = self.complete_server_id(server_query);

                    if !suggestions.is_empty() {
                        let start = if parts.len() == 3 {
                            line.rfind(parts[2]).unwrap_or(pos)
                        } else {
                            pos
                        };
                        let end = pos;

                        return suggestions
                            .into_iter()
                            .map(|mut s| {
                                s.span = Span::new(start, end);
                                s
                            })
                            .collect();
                    }
                }
            }
        }

        // No completions
        Vec::new()
    }
}
