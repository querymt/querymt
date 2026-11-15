//! Unified storage backend abstraction.
//!
//! the `StorageBackend` trait lives in `querymt-agent-core`; `db_path` holds
//! the default database location helpers (runtime layer).

pub mod db_path;

pub use db_path::*;
pub use querymt_agent_core::session::backend::*;
