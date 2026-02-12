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
