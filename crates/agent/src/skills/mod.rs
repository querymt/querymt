//! Agent Skills implementation - agentskills.io specification
//!
//! This module implements lazy-loading skills discovery and execution.
//! Skills are discovered from multiple sources but loaded on-demand via
//! the `skill` tool, keeping initial context small while allowing agents
//! to gain domain-specific knowledge when needed.

pub mod discovery;
pub mod parser;
pub mod permissions;
pub mod registry;
pub mod remote;
pub mod tool;
pub mod types;

use crate::config::SkillsConfig;
use crate::tools::Tool;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use discovery::{default_search_paths, discover_all, discover_from_source};
pub use parser::{SKILL_FILENAME, parse_skill_file};
pub use permissions::{PermissionLevel, SkillPermissions};
pub use registry::SkillRegistry;
pub use tool::SkillTool;
pub use types::{Skill, SkillMetadata, SkillSource, ToolAccessPolicy};

/// Discover configured skills and construct the dynamic tool exposed to an agent.
pub(crate) fn build_skill_tool(config: &SkillsConfig, project_root: &Path) -> Arc<dyn Tool> {
    let mut search_paths = default_search_paths(project_root);
    for custom_path in &config.paths {
        search_paths.push(SkillSource::Configured(custom_path.clone()));
    }

    let mut registry = SkillRegistry::new();
    match registry.load_from_sources(&search_paths, config.include_external) {
        Ok(count) => {
            if count > 0 {
                let compatible_names: Vec<_> = registry
                    .compatible_with(&config.agent_id)
                    .iter()
                    .map(|skill| skill.metadata.name.clone())
                    .collect();
                log::info!(
                    "Skills system initialized: {} skills discovered, {} compatible with agent '{}'",
                    count,
                    compatible_names.len(),
                    config.agent_id
                );
                if !compatible_names.is_empty() {
                    log::debug!("Compatible skills: {}", compatible_names.join(", "));
                }
            } else {
                log::debug!(
                    "Skills system enabled but no skills found in {} search paths",
                    search_paths.len()
                );
            }
        }
        Err(error) => {
            log::warn!(
                "Failed to discover skills: {}. The skill tool will have no discovered skills.",
                error
            );
        }
    }

    Arc::new(SkillTool::new(
        Arc::new(Mutex::new(registry)),
        Some(config.agent_id.clone()),
        Arc::new(config.permissions.clone()),
    ))
}
