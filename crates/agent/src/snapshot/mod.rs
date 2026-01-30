//! Snapshot backend system for filesystem undo/redo support

pub mod backend;
pub mod git;

pub use backend::{GcConfig, GcResult, SnapshotBackend, SnapshotId};
pub use git::GitSnapshotBackend;
