//! Export functionality for agent trajectories in various formats.
//!
//! This module provides support for exporting agent session data to standardized
//! trajectory formats, enabling interoperability with other agent systems and
//! training pipelines.
//!
//! - [`atif`]: Agent Trajectory Interchange Format (ATIF v1.5)
//! - [`sft`]: SFT training data export (OpenAI chat / ShareGPT JSONL)
//! - [`turns`]: Shared turn materialization from event streams

pub mod atif;
pub mod sft;
pub mod turns;

pub use atif::{
    ATIF, ATIFBuilder, AtifAgent, AtifExportOptions, AtifFinalMetrics, AtifMetrics,
    AtifObservation, AtifObservationResult, AtifSource, AtifStep, AtifSubagentTrajectoryRef,
    AtifToolCall, AtifToolDefinition,
};

pub use sft::{SessionFilter, SftExportOptions, SftExportStats, SftFormat};
pub use turns::{SessionMeta, Turn, materialize_turns};
