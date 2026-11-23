//! Bottom layer of the querymt agent.
//!
//! Holds plain types, storage contracts and helpers that every other agent
//! layer depends on. This crate must not depend on any other `querymt-agent-*`
//! crate, nor on `rmcp`, `querymt-utils` or `rusqlite` (design D1 and D7).

pub mod config;
pub mod delegation;
pub mod diff_paths;
pub mod dotagents;
pub mod dotagents_automation;
pub mod dotagents_tasks;
pub mod history;
pub mod hook_context;
pub mod knowledge;
pub mod meta;
pub mod model;
pub mod path;
pub mod projection;
pub mod prompt_text;
pub mod protocol;
pub mod run_lease_path;
pub mod session;
pub mod session_control;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod worktree;

// Wire types live in `querymt-agent-protocol-types`; the old paths keep resolving.
pub use querymt_agent_protocol_types::{
    accounting, agent_mode, dedup, delegation_update, elicitation, events, hash, replay,
    runtime_status, snapshot_id, stream_cursor, verification,
};

pub use diff_paths::DiffPaths;
pub use hook_context::HookContextContribution;

// The wire types come from `agent-client-protocol-schema` directly, while
// `protocol` re-exports them through `agent-client-protocol`. If the two
// resolve to different schema versions this stops compiling.
const _: fn(
    querymt_agent_protocol_types::protocol::SessionNotification,
) -> agent_client_protocol::v1::schema::SessionNotification = |n| n;
