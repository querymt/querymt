//! Unified storage backend abstraction.
//!
//! This module provides a trait-based abstraction over storage backends,
//! enabling clean separation between session persistence (command side)
//! and event/projection handling (query side).

pub use crate::session::storage_backend::StorageBackend;
