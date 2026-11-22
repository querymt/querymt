//! SQLite implementation of the querymt agent storage contracts.
//!
//! This is the agent storage package that talks to `rusqlite`. With
//! `mesh-access`, it also provides a separate mesh policy store.

pub mod backend;
#[cfg(test)]
mod delegation_override_tests;
pub mod error;
pub mod knowledge;
#[cfg(test)]
mod location_tests;
#[cfg(feature = "mesh-access")]
pub mod mesh_access;
pub mod repo_dotagents_automation;
pub mod repo_dotagents_tasks;
pub mod repo_schedule;
pub mod repo_workspace;
pub mod session;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
