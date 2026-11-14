//! Staging area for code that will move to `querymt-agent-sqlite` (task group 3).
//!
//! May depend on core and on `rusqlite`. `git mv` into `crates/agent-sqlite/src`
//! is task 5.1.

pub mod backend;
#[cfg(test)]
mod delegation_override_tests;
pub mod error;
#[cfg(test)]
mod location_tests;
pub mod repo_dotagents_automation;
pub mod repo_dotagents_tasks;
pub mod repo_schedule;
pub mod repo_workspace;
#[cfg(test)]
pub mod test_support;
