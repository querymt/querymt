//! Snapshot backend system for filesystem undo/redo support

pub mod backend;
pub mod git;

pub use backend::{GcConfig, GcResult, SnapshotBackend, SnapshotError, SnapshotId, SnapshotResult};
pub use git::GitSnapshotBackend;
