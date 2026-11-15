use crate::slash_commands::parser::parse_command_file;
use crate::slash_commands::types::{SlashCommand, SlashCommandDiagnostic, SlashCommandSource};
use std::path::{Path, PathBuf};

/// Build discovery sources from include flags and extra configured paths.
///
/// Global: `~/.qmt/commands` and `<config-dir>/commands`
/// Project: `<PROJECT_ROOT>/.qmt/commands`
pub fn search_paths(
    project_root: Option<&Path>,
    include_global: bool,
    include_project: bool,
    extra_paths: &[PathBuf],
) -> Vec<SlashCommandSource> {
    let mut paths = Vec::new();

    if include_global {
        if let Some(home) = dirs::home_dir() {
            push_unique_source(
                &mut paths,
                SlashCommandSource::Global(home.join(".qmt/commands")),
            );
        }
        if let Ok(cfg_dir) = querymt_utils::providers::config_dir() {
            push_unique_source(
                &mut paths,
                SlashCommandSource::Global(cfg_dir.join("commands")),
            );
        }
    }

    if include_project && let Some(root) = project_root {
        push_unique_source(
            &mut paths,
            SlashCommandSource::Project(root.join(".qmt/commands")),
        );
    }

    for path in extra_paths {
        push_unique_source(&mut paths, SlashCommandSource::Configured(path.clone()));
    }

    paths
}

fn source_dir(source: &SlashCommandSource) -> &Path {
    match source {
        SlashCommandSource::Global(path)
        | SlashCommandSource::Project(path)
        | SlashCommandSource::Configured(path) => path,
    }
}

fn push_unique_source(paths: &mut Vec<SlashCommandSource>, source: SlashCommandSource) {
    let path = source_dir(&source);
    if let Some(existing) = paths
        .iter_mut()
        .find(|existing| source_dir(existing) == path)
    {
        if source.priority() > existing.priority() {
            *existing = source;
        }
        return;
    }
    paths.push(source);
}

/// Build default discovery sources for slash commands.
///
/// Global: `~/.qmt/commands`
/// Project: `<PROJECT_ROOT>/.qmt/commands`
pub fn default_search_paths(project_root: &Path) -> Vec<SlashCommandSource> {
    search_paths(Some(project_root), true, true, &[])
}

/// Discover commands from a single source directory.
///
/// Scans for `*.md` files in the directory (non-recursive).
/// Invalid files produce diagnostics instead of failing the entire scan.
pub fn discover_from_source(
    source: &SlashCommandSource,
) -> (Vec<SlashCommand>, Vec<SlashCommandDiagnostic>) {
    let base_path = match source {
        SlashCommandSource::Global(p)
        | SlashCommandSource::Project(p)
        | SlashCommandSource::Configured(p) => p,
    };

    if !base_path.exists() {
        return (Vec::new(), Vec::new());
    }

    let mut commands = Vec::new();
    let mut diagnostics = Vec::new();

    // Scan for .md files directly in the directory (no recursion)
    let entries = match std::fs::read_dir(base_path) {
        Ok(entries) => entries,
        Err(e) => {
            diagnostics.push(SlashCommandDiagnostic {
                path: base_path.clone(),
                message: format!("Failed to read directory: {}", e),
            });
            return (commands, diagnostics);
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();

        // Only process .md files
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        // Skip hidden files (leading dot in stem)
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with('.'))
        {
            continue;
        }

        match parse_command_file(&path, source.clone()) {
            Ok(cmd) => {
                log::debug!("Discovered slash command '/{}' at {:?}", cmd.name, path);
                commands.push(cmd);
            }
            Err(diag) => {
                log::warn!(
                    "Skipping invalid command file {}: {}",
                    diag.path.display(),
                    diag.message
                );
                diagnostics.push(diag);
            }
        }
    }

    (commands, diagnostics)
}

/// Discover commands from multiple sources with deduplication.
///
/// Higher-priority sources override lower-priority ones for the same name.
/// Priority order: Global < Project < Configured.
pub fn discover_all(
    sources: &[SlashCommandSource],
) -> (Vec<SlashCommand>, Vec<SlashCommandDiagnostic>) {
    let mut all_commands = Vec::new();
    let mut all_diagnostics = Vec::new();
    let mut seen: std::collections::HashMap<String, (u8, std::path::PathBuf)> =
        std::collections::HashMap::new();

    for source in sources {
        let (commands, diagnostics) = discover_from_source(source);
        all_diagnostics.extend(diagnostics);

        for cmd in commands {
            let name = cmd.name.clone();
            let priority = cmd.source.priority();

            if let Some((existing_priority, existing_path)) = seen.get(&name) {
                if priority > *existing_priority {
                    log::info!(
                        "Slash command '/{}' from {:?} overrides version from {:?}",
                        name,
                        cmd.path,
                        existing_path
                    );
                    all_commands.retain(|c: &SlashCommand| c.name != name);
                    seen.insert(name.clone(), (priority, cmd.path.clone()));
                    all_commands.push(cmd);
                } else {
                    log::warn!(
                        "Duplicate slash command '/{}' at {:?}, keeping version from {:?}",
                        name,
                        cmd.path,
                        existing_path
                    );
                }
            } else {
                seen.insert(name.clone(), (priority, cmd.path.clone()));
                all_commands.push(cmd);
            }
        }
    }

    (all_commands, all_diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_command(dir: &Path, filename: &str, description: &str, body: &str) {
        fs::write(
            dir.join(filename),
            format!(
                r#"---
description: {}
---
{}"#,
                description, body
            ),
        )
        .unwrap();
    }

    #[test]
    fn test_discover_from_empty_dir() {
        let dir = TempDir::new().unwrap();
        let source = SlashCommandSource::Global(dir.path().to_path_buf());
        let (cmds, diags) = discover_from_source(&source);
        assert!(cmds.is_empty());
        assert!(diags.is_empty());
    }

    #[test]
    fn test_discover_from_nonexistent_dir() {
        let source = SlashCommandSource::Global(PathBuf::from("/nonexistent"));
        let (cmds, diags) = discover_from_source(&source);
        assert!(cmds.is_empty());
        assert!(diags.is_empty());
    }

    #[test]
    fn test_discover_single_command() {
        let dir = TempDir::new().unwrap();
        write_command(
            dir.path(),
            "review.md",
            "Review changes",
            "Review: $ARGUMENTS",
        );

        let source = SlashCommandSource::Global(dir.path().to_path_buf());
        let (cmds, _) = discover_from_source(&source);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "review");
    }

    #[test]
    fn test_discover_multiple_commands() {
        let dir = TempDir::new().unwrap();
        write_command(dir.path(), "review.md", "Review", "Body");
        write_command(dir.path(), "explain.md", "Explain", "Body");
        write_command(dir.path(), "test.md", "Test", "Body");

        let source = SlashCommandSource::Global(dir.path().to_path_buf());
        let (cmds, _) = discover_from_source(&source);
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn test_skips_non_md_files() {
        let dir = TempDir::new().unwrap();
        write_command(dir.path(), "review.md", "Review", "Body");
        fs::write(dir.path().join("notes.txt"), "Not a command").unwrap();
        fs::write(dir.path().join("data.json"), "{}").unwrap();

        let source = SlashCommandSource::Global(dir.path().to_path_buf());
        let (cmds, _) = discover_from_source(&source);
        assert_eq!(cmds.len(), 1);
    }

    #[test]
    fn test_skips_hidden_files() {
        let dir = TempDir::new().unwrap();
        write_command(dir.path(), "visible.md", "Visible", "Body");
        fs::write(
            dir.path().join(".hidden.md"),
            "---\ndescription: Hidden\n---\nBody",
        )
        .unwrap();

        let source = SlashCommandSource::Global(dir.path().to_path_buf());
        let (cmds, _) = discover_from_source(&source);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "visible");
    }

    #[test]
    fn test_project_overrides_global() {
        let global_dir = TempDir::new().unwrap();
        let project_dir = TempDir::new().unwrap();

        write_command(
            global_dir.path(),
            "review.md",
            "Global review",
            "Global body",
        );
        write_command(
            project_dir.path(),
            "review.md",
            "Project review",
            "Project body",
        );

        let sources = vec![
            SlashCommandSource::Global(global_dir.path().to_path_buf()),
            SlashCommandSource::Project(project_dir.path().to_path_buf()),
        ];

        let (cmds, _) = discover_all(&sources);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].description, "Project review");
    }

    #[test]
    fn test_configured_overrides_project() {
        let project_dir = TempDir::new().unwrap();
        let config_dir = TempDir::new().unwrap();

        write_command(project_dir.path(), "review.md", "Project review", "Body");
        write_command(config_dir.path(), "review.md", "Configured review", "Body");

        let sources = vec![
            SlashCommandSource::Project(project_dir.path().to_path_buf()),
            SlashCommandSource::Configured(config_dir.path().to_path_buf()),
        ];

        let (cmds, _) = discover_all(&sources);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].description, "Configured review");
    }

    #[test]
    fn test_invalid_file_produces_diagnostic() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("bad.md"), "# No frontmatter\n").unwrap();

        let source = SlashCommandSource::Global(dir.path().to_path_buf());
        let (cmds, diags) = discover_from_source(&source);
        assert!(cmds.is_empty());
        assert_eq!(diags.len(), 1);
        assert!(diags[0].message.contains("Missing YAML frontmatter"));
    }

    #[test]
    fn test_default_search_paths() {
        let paths = default_search_paths(Path::new("/my/project"));
        // Should contain at least the project path
        assert!(paths.iter().any(
            |p| matches!(p, SlashCommandSource::Project(pth) if pth.ends_with(".qmt/commands"))
        ));
    }

    #[test]
    fn test_search_paths_honors_include_flags_and_extra_paths() {
        let extra = PathBuf::from("/custom/commands");
        let none = search_paths(Some(Path::new("/my/project")), false, false, &[]);
        assert!(none.is_empty());

        let configured = search_paths(None, false, true, std::slice::from_ref(&extra));
        assert_eq!(configured.len(), 1);
        assert!(matches!(
            &configured[0],
            SlashCommandSource::Configured(path) if path == &extra
        ));

        let project_only = search_paths(Some(Path::new("/my/project")), false, true, &[]);
        assert_eq!(project_only.len(), 1);
        assert!(matches!(
            &project_only[0],
            SlashCommandSource::Project(path) if path.ends_with(".qmt/commands")
        ));
    }

    #[test]
    fn test_search_paths_dedupes_equivalent_directories() {
        let extra = PathBuf::from("/my/project/.qmt/commands");
        let paths = search_paths(
            Some(Path::new("/my/project")),
            false,
            true,
            std::slice::from_ref(&extra),
        );
        assert_eq!(paths.len(), 1);
        assert!(matches!(
            &paths[0],
            SlashCommandSource::Configured(path) if path == &extra
        ));

        let global = search_paths(None, true, false, &[]);
        let mut seen = std::collections::HashSet::new();
        for source in &global {
            assert!(
                seen.insert(source_dir(source).to_path_buf()),
                "duplicate discovery path {:?}",
                source_dir(source)
            );
        }
    }
}
