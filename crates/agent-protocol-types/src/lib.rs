//! Plain wire types that QueryMT agents and clients exchange.
//!
//! Agent events, the load-snapshot payload and its parts, and session
//! input-state notifications. This crate is serde and typeshare only: it has no
//! async runtime and builds for `wasm32-unknown-unknown`, so a browser client and
//! the agent depend on the same definitions.

pub mod accounting;
pub mod agent_mode;
pub mod dedup;
pub mod delegation_update;
pub mod elicitation;
pub mod events;
pub mod hash;
pub mod protocol;
pub mod replay;
pub mod runtime_status;
pub mod session;
pub mod snapshot_id;
pub mod stream_cursor;
pub mod verification;
pub mod worktree;
