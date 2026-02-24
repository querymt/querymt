// Remote skill fetching - Phase 2 implementation
// This is a placeholder for future remote skill support

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

/// Remote skill index format
#[derive(Debug, Deserialize)]
pub struct SkillIndex {
    pub skills: Vec<SkillReference>,
}

#[derive(Debug, Deserialize)]
pub struct SkillReference {
    pub name: String,
    pub description: String,
    pub url: String, // URL to SKILL.md
    pub version: Option<String>,
}

/// Fetch skill index from remote URL (Phase 2 - not yet implemented)
pub async fn fetch_skill_index(_url: &str) -> Result<SkillIndex> {
    anyhow::bail!("Remote skill fetching not yet implemented")
}

/// Download and cache a remote skill (Phase 2 - not yet implemented)
pub async fn download_skill(_reference: &SkillReference, _cache_dir: &PathBuf) -> Result<PathBuf> {
    anyhow::bail!("Remote skill downloading not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_reference_construction() {
        let r = SkillReference {
            name: "my-skill".to_string(),
            description: "A helpful skill".to_string(),
            url: "https://example.com/SKILL.md".to_string(),
            version: Some("1.0.0".to_string()),
        };
        assert_eq!(r.name, "my-skill");
        assert_eq!(r.description, "A helpful skill");
        assert_eq!(r.version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn skill_reference_without_version() {
        let r = SkillReference {
            name: "no-version".to_string(),
            description: "desc".to_string(),
            url: "https://example.com/skill.md".to_string(),
            version: None,
        };
        assert!(r.version.is_none());
    }

    #[test]
    fn skill_index_construction() {
        let idx = SkillIndex {
            skills: vec![
                SkillReference {
                    name: "skill-a".to_string(),
                    description: "First".to_string(),
                    url: "https://example.com/a.md".to_string(),
                    version: None,
                },
                SkillReference {
                    name: "skill-b".to_string(),
                    description: "Second".to_string(),
                    url: "https://example.com/b.md".to_string(),
                    version: Some("2.0".to_string()),
                },
            ],
        };
        assert_eq!(idx.skills.len(), 2);
        assert_eq!(idx.skills[0].name, "skill-a");
        assert_eq!(idx.skills[1].name, "skill-b");
    }

    #[tokio::test]
    async fn fetch_skill_index_returns_not_implemented_error() {
        let result = fetch_skill_index("https://example.com/index.json").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not yet implemented"));
    }

    #[tokio::test]
    async fn download_skill_returns_not_implemented_error() {
        let reference = SkillReference {
            name: "test".to_string(),
            description: "test skill".to_string(),
            url: "https://example.com/SKILL.md".to_string(),
            version: None,
        };
        let cache_dir = PathBuf::from("/tmp/cache");
        let result = download_skill(&reference, &cache_dir).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not yet implemented"));
    }
}
