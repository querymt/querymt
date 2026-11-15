//! Bottom layer of the querymt agent.
//!
//! Holds plain types, storage contracts and helpers that every other agent
//! layer depends on. This crate must not depend on any other `querymt-agent-*`
//! crate, nor on `rmcp`, `querymt-utils` or `rusqlite` (design D1 and D7).

pub mod accounting;
pub mod agent_mode;
pub mod config;
pub mod dedup;
pub mod delegation;
pub mod delegation_update;
pub mod diff_paths;
pub mod dotagents;
pub mod dotagents_automation;
pub mod dotagents_tasks;
pub mod elicitation;
pub mod events;
pub mod hash;
pub mod history;
pub mod hook_context;
pub mod knowledge;
pub mod meta;
pub mod model;
pub mod path;
pub mod prompt_text;
pub mod protocol;
pub mod run_lease_path;
pub mod runtime_status;
pub mod session;
pub mod session_control;
pub mod snapshot_id;
pub mod stream_cursor;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod verification;
pub mod worktree;

pub use diff_paths::DiffPaths;
pub use hook_context::HookContextContribution;
