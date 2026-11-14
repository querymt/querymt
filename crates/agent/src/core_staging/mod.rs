//! Staging area for code that will move to `querymt-agent-core` (task group 3).
//!
//! Everything here may depend only on the core layer. Old paths re-export the
//! moved items, so no caller changes. `git mv` of this directory into
//! `crates/agent-core/src` is task 4.1.

pub mod history;
pub mod prompt_text;
pub mod protocol;
pub mod run_lease_path;
pub mod stream_cursor;
