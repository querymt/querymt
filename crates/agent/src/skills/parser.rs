use crate::skills::types::{Skill, SkillMetadata, SkillSource};
use anyhow::{Context, Result, bail};
use std::path::Path;

pub const SKILL_FILENAME: &str = "SKILL.md";

/// Lowercase `.agents` Protocol spelling of the skill definition file.
///
/// Only honored inside `.agents/skills` discovery sources; every other source
/// keeps requiring the uppercase `SKILL.md` name.
pub const PROTOCOL_SKILL_FILENAME: &str = "skill.md";

/// Parse a SKILL.md file into a Skill struct.
///
/// This is the legacy, non-protocol entry point: `name` and `description`
/// are required and protocol-only metadata (`id`, `enabled`) is ignored for
/// behavior. Use [`parse_skill_file_ex`] for `.agents/skills` sources.
pub fn parse_skill_file(path: &Path, source: SkillSource) -> Result<Skill> {
    parse_skill_file_ex(path, source, false)
}

/// Parse a skill definition file with `.agents` Protocol extensions.
///
/// Protocol mode applies to files discovered under `.agents/skills` and:
/// - accepts the lowercase `skill.md` spelling (the caller filters names),
/// - honors the `id` and `enabled` frontmatter metadata,
/// - defaults a missing `name` to the explicit `id` and then to the entry
///   directory name, matching the protocol's stable-ID rule.
///
/// `description` remains required in both modes.
pub fn parse_skill_file_ex(path: &Path, source: SkillSource, protocol: bool) -> Result<Skill> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let parsed = gray_matter::Matter::<gray_matter::engine::YAML>::new()
        .parse::<gray_matter::Pod>(&content)
        .with_context(|| format!("Failed to parse file {}", path.display()))?;

    // Extract and validate frontmatter
    let data = parsed
        .data
        .ok_or_else(|| anyhow::anyhow!("Missing YAML frontmatter in {}", path.display()))?;
    let mut metadata: SkillMetadata = serde_path_to_error::deserialize(&data).map_err(|error| {
        let field = error.path().to_string();
        anyhow::Error::new(error.into_inner())
            .context(format!("Failed to deserialize skill metadata at {field}"))
    })?;

    if protocol {
        apply_protocol_defaults(&mut metadata, path);
    }

    // Validate required fields
    if metadata.name.trim().is_empty() {
        bail!(
            "Skill 'name' is required and cannot be empty in {}",
            path.display()
        );
    }
    if metadata.description.trim().is_empty() {
        bail!(
            "Skill 'description' is required and cannot be empty in {}",
            path.display()
        );
    }

    let skill_dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine skill directory from {}", path.display()))?
        .to_path_buf();

    Ok(Skill {
        path: skill_dir,
        metadata,
        content: parsed.content,
        source,
    })
}

/// Fill `.agents` Protocol defaults: the stable ID falls back to the entry
/// directory name and a missing display name falls back to that ID.
fn apply_protocol_defaults(metadata: &mut SkillMetadata, path: &Path) {
    let entry_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::to_string);
    if metadata.id.as_ref().is_some_and(|id| id.trim().is_empty()) {
        metadata.id = None;
    }
    if let Some(entry_id) = entry_id {
        if metadata.id.is_none() {
            metadata.id = Some(entry_id);
        }
        if metadata.name.trim().is_empty() {
            metadata.name = metadata.id.clone().unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_parse_valid_skill() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
name: test-skill
description: A test skill
allowed-tools: read_tool
---
# Test Skill Content
"#,
        )
        .unwrap();

        let skill =
            parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf())).unwrap();
        assert_eq!(skill.metadata.name, "test-skill");
        assert!(skill.content.contains("Test Skill Content"));
    }

    #[test]
    fn test_missing_required_fields() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
name: test
---
Content
"#,
        )
        .unwrap();

        let result = parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
        // The error will be about parsing or missing description field
        // Accept either as valid - the important thing is that it fails
    }

    #[test]
    fn test_empty_name() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
name: ""
description: Test
---
Content
"#,
        )
        .unwrap();

        let result = parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cannot be empty"));
    }

    #[test]
    fn test_missing_frontmatter() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(&skill_path, "# Just content, no frontmatter\n").unwrap();

        let result = parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing YAML frontmatter")
        );
    }

    #[test]
    fn test_invalid_compatibility_reports_field_path() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
name: test-skill
description: A test skill
compatibility: ["*"]
---
Content
"#,
        )
        .unwrap();

        let error = parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf()))
            .unwrap_err();
        let error_chain = format!("{error:#}");
        assert!(
            error_chain.contains(
                "Failed to deserialize skill metadata at compatibility: Type error, expected: string"
            ),
            "unexpected error: {error_chain}"
        );
    }

    #[test]
    fn test_invalid_allowed_tools_reports_field_path() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
name: test-skill
description: A test skill
allowed-tools: ["read_tool"]
---
Content
"#,
        )
        .unwrap();

        let error = parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf()))
            .unwrap_err();
        let error_chain = format!("{error:#}");
        assert!(
            error_chain.contains(
                "Failed to deserialize skill metadata at allowed-tools: Type error, expected: string"
            ),
            "unexpected error: {error_chain}"
        );
    }

    #[test]
    fn test_parse_with_optional_fields() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
name: advanced-skill
description: Advanced test skill
version: "1.0.0"
license: MIT
author: Test Author
tags: ["development", "testing"]
compatibility: Requires git and network access.
allowed-tools: read_tool write_file
---
# Advanced Skill

This skill has all optional fields.
"#,
        )
        .unwrap();

        let skill =
            parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf())).unwrap();
        assert_eq!(skill.metadata.name, "advanced-skill");
        assert_eq!(skill.metadata.version, Some("1.0.0".to_string()));
        assert_eq!(skill.metadata.license, Some("MIT".to_string()));
        assert_eq!(skill.metadata.author, Some("Test Author".to_string()));
        assert_eq!(
            skill.metadata.compatibility.as_deref(),
            Some("Requires git and network access.")
        );
        assert_eq!(
            skill.metadata.allowed_tools.as_deref(),
            Some("read_tool write_file")
        );
        assert_eq!(
            skill.metadata.tags,
            Some(vec!["development".to_string(), "testing".to_string()])
        );
        assert!(skill.content.contains("This skill has all optional fields"));
    }

    #[test]
    fn test_parse_openspec_frontmatter() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
name: openspec-propose
description: Propose a new change with all artifacts generated in one step.
allowed-tools: Bash(openspec:*)
license: MIT
compatibility: Requires openspec CLI.
metadata:
  author: openspec
  version: "1.0"
---
OpenSpec instructions.
"#,
        )
        .unwrap();

        let skill =
            parse_skill_file(&skill_path, SkillSource::Project(dir.path().to_path_buf())).unwrap();
        assert_eq!(skill.metadata.name, "openspec-propose");
        assert_eq!(
            skill.metadata.compatibility.as_deref(),
            Some("Requires openspec CLI.")
        );
        assert_eq!(
            skill.metadata.allowed_tools.as_deref(),
            Some("Bash(openspec:*)")
        );
        assert_eq!(skill.metadata.extra["metadata"]["author"], "openspec");
        assert!(skill.content.contains("OpenSpec instructions."));
    }

    #[test]
    fn test_protocol_skill_defaults_name_to_entry_directory() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("skill.md");
        fs::write(
            &skill_path,
            r#"---
description: Reviews code changes.
---
Review instructions.
"#,
        )
        .unwrap();

        let skill = parse_skill_file_ex(
            &skill_path,
            SkillSource::Project(dir.path().to_path_buf()),
            true,
        )
        .unwrap();
        // Directory here is the temp dir itself; protocol mode defaults the
        // ID to the entry directory name.
        assert_eq!(
            skill.metadata.id,
            Some(
                dir.path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            )
        );
        assert_eq!(
            skill.metadata.name,
            dir.path().file_name().unwrap().to_string_lossy()
        );
        assert_eq!(skill.metadata.effective_id(), skill.metadata.name);
        assert!(skill.content.contains("Review instructions."));
    }

    #[test]
    fn test_protocol_skill_defaults_name_to_explicit_id() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("skill.md");
        fs::write(
            &skill_path,
            r#"---
id: custom-review
description: Reviews code changes.
enabled: false
---
Review instructions.
"#,
        )
        .unwrap();

        let skill = parse_skill_file_ex(
            &skill_path,
            SkillSource::Project(dir.path().to_path_buf()),
            true,
        )
        .unwrap();
        assert_eq!(skill.metadata.id.as_deref(), Some("custom-review"));
        assert_eq!(skill.metadata.name, "custom-review");
        assert!(!skill.metadata.is_enabled());
    }

    #[test]
    fn test_protocol_skill_treats_blank_id_as_absent() {
        let dir = TempDir::new().unwrap();
        let entry = dir.path().join("review");
        fs::create_dir_all(&entry).unwrap();
        let path = entry.join(PROTOCOL_SKILL_FILENAME);
        fs::write(
            &path,
            "---\nid: '   '\nname: ''\ndescription: Reviews code\n---\nBody\n",
        )
        .unwrap();

        let skill = parse_skill_file_ex(&path, SkillSource::Project(entry.clone()), true).unwrap();
        assert_eq!(skill.metadata.id.as_deref(), Some("review"));
        assert_eq!(skill.metadata.name, "review");
    }

    #[test]
    fn test_non_protocol_parse_still_requires_name() {
        let dir = TempDir::new().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        fs::write(
            &skill_path,
            r#"---
description: No name here.
---
Content
"#,
        )
        .unwrap();

        let result = parse_skill_file(&skill_path, SkillSource::Global(dir.path().to_path_buf()));
        assert!(result.is_err());
    }
}
