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

pub use discovery::{default_search_paths, discover_all, discover_from_source};
pub use parser::{SKILL_FILENAME, parse_skill_file};
pub use permissions::{PermissionLevel, SkillPermissions};
pub use registry::SkillRegistry;
pub use tool::SkillTool;
pub use types::{Skill, SkillMetadata, SkillSource, ToolAccessPolicy};
