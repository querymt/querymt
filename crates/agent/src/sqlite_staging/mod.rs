//! Staging area for code that will move to `querymt-agent-sqlite` (task group 3).
//!
//! May depend on core and on `rusqlite`. `git mv` into `crates/agent-sqlite/src`
//! is task 5.1.

pub mod backend;
pub mod error;
pub mod repo_dotagents_automation;
pub mod repo_dotagents_tasks;
pub mod repo_schedule;
pub mod repo_workspace;
