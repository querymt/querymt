use crate::skills::types::{Skill, SkillMetadata, SkillSource};
use anyhow::{Context, Result, bail};
use std::path::Path;

pub const SKILL_FILENAME: &str = "SKILL.md";

/// Parse a SKILL.md file into a Skill struct
pub fn parse_skill_file(path: &Path, source: SkillSource) -> Result<Skill> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let parsed = gray_matter::Matter::<gray_matter::engine::YAML>::new()
        .parse::<SkillMetadata>(&content)
        .with_context(|| format!("Failed to parse file {}", path.display()))?;

    // Extract and validate frontmatter
    let metadata: SkillMetadata = parsed
        .data
        .ok_or_else(|| anyhow::anyhow!("Missing YAML frontmatter in {}", path.display()))?;

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
allowed-tools: ["read_file"]
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
compatibility: ["querymt", "claude-code"]
allowed-tools: ["read_file", "write_file"]
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
            skill.metadata.tags,
            Some(vec!["development".to_string(), "testing".to_string()])
        );
        assert!(skill.content.contains("This skill has all optional fields"));
    }
}
