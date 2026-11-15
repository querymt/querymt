//! Services layer of the querymt agent.
//!
//! Accounting, managed worktrees, snapshots, export, the built-in tools that
//! only need the `ToolContext` contract, verification and the event plumbing.
//! Depends on `querymt-agent-core`, `-index` and `-knowledge` (design D1); the
//! SQLite implementation is a dev-dependency used by tests.

pub(crate) use querymt_agent_core as core_staging;
pub(crate) use querymt_agent_core::{elicitation, events, hash, model};
pub(crate) use querymt_agent_index as index;

/// Contract types from core plus the repository-knowledge crate, at their old
/// paths.
pub(crate) mod knowledge {
    pub use querymt_agent_core::knowledge::*;
    pub use querymt_agent_knowledge as repository;
    #[cfg(test)]
    pub use querymt_agent_sqlite::knowledge::sqlite;
}

pub mod accounting;
pub mod control;
pub mod event_fanout;
pub mod event_sink;
pub mod export;
pub mod model_heuristics;
pub mod model_info;
pub mod session;
pub mod session_event_stream;
pub mod snapshot;
pub mod tasks;
pub mod tools;
pub mod verification;
pub mod workspace_query;
pub mod worktree;
