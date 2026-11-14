//! Unified storage backend abstraction.
//!
//! `contract` holds the `StorageBackend` trait (core layer); `db_path` holds
//! the default database location helpers (runtime layer).

pub mod contract;
pub mod db_path;

pub use contract::*;
pub use db_path::*;
