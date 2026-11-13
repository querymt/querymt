//! Bottom layer of the querymt agent.
//!
//! Holds plain types and helpers that every other agent layer depends on.
//! This crate must not depend on any other `querymt-agent-*` crate.

pub mod diff_paths;
pub mod hash;
pub mod hook_context;
pub mod path;

pub use diff_paths::DiffPaths;
pub use hook_context::HookContextContribution;
