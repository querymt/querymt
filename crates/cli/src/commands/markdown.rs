use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Metadata for a markdown command (from YAML frontmatter)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommandMetadata {
    /// Description of what the command does
    #[serde(default)]
    pub description: Option<String>,

    /// Hint showing expected arguments (e.g., "add [server-id] | list")
    #[serde(rename = "argument-hint")]
    #[serde(default)]
    pub argument_hint: Option<String>,

    /// Override the model to use for this command
    #[serde(default)]
    pub model: Option<String>,

    /// List of allowed tools this command can use
    #[serde(rename = "allowed-tools")]
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,

    /// Prevent automatic model invocation
    #[serde(rename = "disable-model-invocation")]
    #[serde(default)]
    pub disable_model_invocation: bool,
}

/// A command loaded from a markdown file
#[derive(Debug, Clone)]
pub struct MarkdownCommand {
    name: String,
    metadata: CommandMetadata,
    content: String,
}

impl MarkdownCommand {
    /// Load a markdown command from a file
    pub fn from_file(path: PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read file: {:?}", path))?;

        // Extract command name from filename
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("Invalid filename")?
            .to_string();

        Self::from_string(name, content)
    }

    /// Parse a markdown command from a string
    pub fn from_string(name: String, content: String) -> Result<Self> {
        let (metadata, content) = Self::parse_frontmatter(&content)?;

        Ok(Self {
            name,
            metadata,
            content,
        })
    }

    /// Parse YAML frontmatter from markdown content
    fn parse_frontmatter(content: &str) -> Result<(CommandMetadata, String)> {
        let content = content.trim();

        // Check if content starts with frontmatter delimiter
        if !content.starts_with("---") {
            return Ok((CommandMetadata::default(), content.to_string()));
        }

        // Find the closing delimiter
        let lines: Vec<&str> = content.lines().collect();
        let mut end_idx = None;

        for (idx, line) in lines.iter().enumerate().skip(1) {
            if line.trim() == "---" {
                end_idx = Some(idx);
                break;
            }
        }

        match end_idx {
            Some(idx) => {
                // Extract frontmatter YAML
                let frontmatter = lines[1..idx].join("\n");
                let remaining_content = lines[idx + 1..].join("\n");

                // Parse YAML
                let metadata: CommandMetadata = serde_yaml::from_str(&frontmatter)
                    .context("Failed to parse YAML frontmatter")?;

                Ok((metadata, remaining_content.trim().to_string()))
            }
            None => {
                // No closing delimiter, treat as plain content
                Ok((CommandMetadata::default(), content.to_string()))
            }
        }
    }

    /// Get the command name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the command description
    pub fn description(&self) -> &str {
        self.metadata
            .description
            .as_deref()
            .unwrap_or("Custom command")
    }

    /// Get the usage hint
    pub fn usage(&self) -> &str {
        self.metadata
            .argument_hint
            .as_deref()
            .unwrap_or("")
    }

    /// Get the command content/prompt
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Get the metadata
    pub fn metadata(&self) -> &CommandMetadata {
        &self.metadata
    }

    /// Substitute arguments into the command content
    /// Supports:
    /// - $ARGUMENTS: all arguments as a single string
    /// - $1, $2, $3, ...: individual positional arguments
    pub fn substitute_arguments(&self, args: &[String]) -> String {
        let mut result = self.content.clone();

        // Substitute $ARGUMENTS with all args joined by space
        let all_args = args.join(" ");
        result = result.replace("$ARGUMENTS", &all_args);

        // Substitute positional arguments $1, $2, etc.
        for (idx, arg) in args.iter().enumerate() {
            let placeholder = format!("${}", idx + 1);
            result = result.replace(&placeholder, arg);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter() {
        let content = r#"---
description: "Test command"
argument-hint: "[file]"
model: "claude-sonnet-4"
---

This is the command content with $1 argument."#;

        let cmd = MarkdownCommand::from_string("test".to_string(), content.to_string()).unwrap();

        assert_eq!(cmd.name(), "test");
        assert_eq!(cmd.description(), "Test command");
        assert_eq!(cmd.usage(), "[file]");
        assert_eq!(cmd.metadata().model, Some("claude-sonnet-4".to_string()));
        assert!(cmd.content().contains("This is the command content"));
    }

    #[test]
    fn test_parse_no_frontmatter() {
        let content = "Just plain markdown content.";

        let cmd = MarkdownCommand::from_string("test".to_string(), content.to_string()).unwrap();

        assert_eq!(cmd.name(), "test");
        assert_eq!(cmd.description(), "Custom command");
        assert_eq!(cmd.content(), "Just plain markdown content.");
    }

    #[test]
    fn test_substitute_arguments() {
        let content = "Process $1 and $2, all args: $ARGUMENTS";
        let cmd = MarkdownCommand::from_string("test".to_string(), content.to_string()).unwrap();

        let result = cmd.substitute_arguments(&["file.rs".to_string(), "output.txt".to_string()]);

        assert_eq!(result, "Process file.rs and output.txt, all args: file.rs output.txt");
    }
}
