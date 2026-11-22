//! SQLite implementation of the querymt agent storage contracts.
//!
//! Depends only on `querymt-agent-core` (design D1). This is the one agent
//! package, besides `querymt-agent-knowledge`'s own cache file, that talks to
//! `rusqlite`.

pub mod backend;
#[cfg(test)]
mod delegation_override_tests;
pub mod error;
pub mod knowledge;
#[cfg(test)]
mod location_tests;
pub mod repo_dotagents_automation;
pub mod repo_dotagents_tasks;
pub mod repo_schedule;
pub mod repo_workspace;
pub mod session;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
