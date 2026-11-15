use crate::slash_commands::types::{
    CommandFrontmatter, SlashCommand, SlashCommandDiagnostic, SlashCommandSource,
    is_valid_command_name,
};
use anyhow::Result;
use std::path::Path;

/// Parse a command `.md` file into a [`SlashCommand`].
///
/// The filename (without `.md`) becomes the command name.
/// The file must contain YAML frontmatter with at least a `description` field.
/// The body (after frontmatter) is the prompt template.
pub fn parse_command_file(
    path: &Path,
    source: SlashCommandSource,
) -> Result<SlashCommand, SlashCommandDiagnostic> {
    let content = std::fs::read_to_string(path).map_err(|e| SlashCommandDiagnostic {
        path: path.to_path_buf(),
        message: format!("Failed to read file: {}", e),
    })?;

    // Derive command name from filename (without .md extension)
    let file_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    if !is_valid_command_name(&file_name) {
        return Err(SlashCommandDiagnostic {
            path: path.to_path_buf(),
            message: format!(
                "Invalid command name derived from filename: '{}'. \
                 Must match [a-zA-Z][a-zA-Z0-9_-]*",
                file_name
            ),
        });
    }

    let parsed = gray_matter::Matter::<gray_matter::engine::YAML>::new()
        .parse::<CommandFrontmatter>(&content)
        .map_err(|e| SlashCommandDiagnostic {
            path: path.to_path_buf(),
            message: format!("Failed to parse file: {}", e),
        })?;

    // Extract and validate frontmatter
    let metadata: CommandFrontmatter = parsed.data.ok_or_else(|| SlashCommandDiagnostic {
        path: path.to_path_buf(),
        message: "Missing YAML frontmatter".to_string(),
    })?;

    // Validate required description
    if metadata.description.trim().is_empty() {
        return Err(SlashCommandDiagnostic {
            path: path.to_path_buf(),
            message: "Field 'description' is required and cannot be empty".to_string(),
        });
    }

    let template = parsed.content.trim().to_string();

    // Validate script path is not absolute (security)
    if let Some(ref script) = metadata.script
        && script.path.is_absolute()
    {
        return Err(SlashCommandDiagnostic {
            path: path.to_path_buf(),
            message: format!(
                "Script path must be relative to the command file directory, got: {}",
                script.path.display()
            ),
        });
    }

    Ok(SlashCommand {
        name: file_name,
        source,
        path: path.to_path_buf(),
        description: metadata.description.trim().to_string(),
        argument_hint: metadata.argument_hint,
        tags: metadata.tags,
        template,
        script: metadata.script,
        requires_script: metadata.requires_script,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_command(dir: &Path, filename: &str, content: &str) -> PathBuf {
        let path = dir.join(filename);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_parse_valid_command() {
        let dir = TempDir::new().unwrap();
        let path = write_command(
            dir.path(),
            "review.md",
            r#"---
description: Review the current changes
argument-hint: "[scope]"
tags: ["review", "code"]
---
Review the changes in scope: $ARGUMENTS
"#,
        );

        let cmd = parse_command_file(&path, SlashCommandSource::Global(dir.path().to_path_buf()))
            .unwrap();
        assert_eq!(cmd.name, "review");
        assert_eq!(cmd.description, "Review the current changes");
        assert_eq!(cmd.argument_hint, Some("[scope]".to_string()));
        assert_eq!(cmd.tags, vec!["review", "code"]);
        assert!(cmd.template.contains("$ARGUMENTS"));
    }

    #[test]
    fn test_parse_minimal_command() {
        let dir = TempDir::new().unwrap();
        let path = write_command(
            dir.path(),
            "help.md",
            r#"---
description: Show help
---
This is the help text.
"#,
        );

        let cmd = parse_command_file(&path, SlashCommandSource::Global(dir.path().to_path_buf()))
            .unwrap();
        assert_eq!(cmd.name, "help");
        assert_eq!(cmd.description, "Show help");
        assert_eq!(cmd.argument_hint, None);
        assert!(cmd.tags.is_empty());
        assert!(cmd.script.is_none());
        assert!(!cmd.requires_script);
    }

    #[test]
    fn test_parse_command_with_script() {
        let dir = TempDir::new().unwrap();
        let path = write_command(
            dir.path(),
            "analyze.md",
            r#"---
description: Analyze a stack trace
script:
  runtime: python
  path: scripts/parse_trace.py
  mode: generate-prompt
  timeout-ms: 2000
requires-script: true
---
Use the parsed output: $ARGUMENTS
"#,
        );

        let cmd = parse_command_file(&path, SlashCommandSource::Project(dir.path().to_path_buf()))
            .unwrap();
        assert_eq!(cmd.name, "analyze");
        assert!(cmd.script.is_some());
        assert!(cmd.requires_script);
        let script = cmd.script.unwrap();
        assert_eq!(script.path, PathBuf::from("scripts/parse_trace.py"));
    }

    #[test]
    fn test_missing_frontmatter() {
        let dir = TempDir::new().unwrap();
        let path = write_command(dir.path(), "bad.md", "# Just content\n");

        let result =
            parse_command_file(&path, SlashCommandSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("Missing YAML frontmatter")
        );
    }

    #[test]
    fn test_empty_description() {
        let dir = TempDir::new().unwrap();
        let path = write_command(
            dir.path(),
            "nodesc.md",
            r#"---
description: ""
---
Content
"#,
        );

        let result =
            parse_command_file(&path, SlashCommandSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("description"));
    }

    #[test]
    fn test_invalid_filename() {
        let dir = TempDir::new().unwrap();
        let path = write_command(
            dir.path(),
            "123bad.md",
            r#"---
description: Bad name
---
Content
"#,
        );

        let result =
            parse_command_file(&path, SlashCommandSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Invalid command name"));
    }

    #[test]
    fn test_absolute_script_path_rejected() {
        let dir = TempDir::new().unwrap();
        let path = write_command(
            dir.path(),
            "evil.md",
            r#"---
description: Evil
script:
  runtime: python
  path: /tmp/evil.py
---
Content
"#,
        );

        let result =
            parse_command_file(&path, SlashCommandSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("relative"));
    }

    #[test]
    fn test_project_source() {
        let dir = TempDir::new().unwrap();
        let path = write_command(
            dir.path(),
            "test-cmd.md",
            r#"---
description: A test
---
Body
"#,
        );

        let cmd = parse_command_file(&path, SlashCommandSource::Project(dir.path().to_path_buf()))
            .unwrap();
        assert_eq!(
            cmd.source,
            SlashCommandSource::Project(dir.path().to_path_buf())
        );
        assert_eq!(cmd.name, "test-cmd");
    }

    #[test]
    fn test_nonexistent_file() {
        let result = parse_command_file(
            Path::new("/nonexistent/path.md"),
            SlashCommandSource::Global(PathBuf::from("/nonexistent")),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Failed to read"));
    }
}
