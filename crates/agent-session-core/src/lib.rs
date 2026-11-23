//! Deterministic session view reducer.
//!
//! Turns the agent's session output (load snapshot, ACP `session/update`
//! notifications, older-history pages) and client observations into one
//! conversation view with change reports. It has no async runtime and builds
//! for `wasm32-unknown-unknown`.

pub mod bounded;
pub mod entry;
pub mod input;
pub mod view;

#[cfg(feature = "test-support")]
pub mod testing;

mod client;
mod core;
mod cursors;
mod decode;
mod history;
mod project;
mod snapshot;
mod store;
mod subagents;
mod updates;
mod usage;

pub use crate::core::SessionCore;

/// Version of the input and output shapes. Bumped whenever a shape changes in a
/// way old clients cannot read.
///
/// 2: subagents replace delegations (`header.subagents`, no `delegation_update` input).
pub const SCHEMA_VERSION: u32 = 2;
