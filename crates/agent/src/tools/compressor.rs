//! Tool output compression strategies (Layer 1).
//!
//! This module defines the [`ToolOutputCompressor`] trait — a strategy interface
//! for compressing tool output before it enters the conversation history.
//!
//! Two implementations ship out of the box:
//!
//! - [`TruncationCompressor`](super::compressor_truncation::TruncationCompressor):
//!   blind head/tail truncation (the original default behaviour).
//! - [`SqueezCompressor`](super::compressor_squeez::SqueezCompressor):
//!   LLM-powered intelligent extraction using a squeez model.
//!
//! The active strategy is selected via [`ToolOutputStrategy`](crate::config::ToolOutputStrategy)
//! in the agent config and stored as `Arc<dyn ToolOutputCompressor>` on
//! [`AgentConfig`](crate::agent::agent_config::AgentConfig).

use async_trait::async_trait;
use querymt::chat::Content;

/// Result of tool output compression.
#[derive(Debug, Clone)]
pub struct CompressionResult {
    /// The compressed content blocks (replaces the original text blocks).
    pub blocks: Vec<Content>,
    /// Whether any compression was actually applied.
    pub was_compressed: bool,
    /// Original line count before compression.
    pub original_lines: usize,
    /// Original byte count before compression.
    pub original_bytes: usize,
}

/// Contextual metadata passed to the compressor so it can make informed
/// decisions (e.g. build a task description for squeez).
pub struct CompressionContext<'a> {
    /// Name of the tool that produced this output.
    pub tool_name: &'a str,
    /// Raw JSON string of the tool call arguments.
    pub tool_arguments: &'a str,
    /// Unique tool-call identifier (used for overflow file naming).
    pub tool_call_id: &'a str,
    /// Session identifier.
    pub session_id: &'a str,
    /// Optional per-tool hint appended when truncation occurs.
    pub tool_hint: Option<&'a str>,
}

/// Strategy for compressing/filtering tool output before it enters the
/// conversation history.
///
/// Implementations **must** be infallible from the caller's perspective:
/// if an internal error occurs, fall back to returning the input unchanged
/// (i.e. `was_compressed = false`).
#[async_trait]
pub trait ToolOutputCompressor: Send + Sync {
    /// Compress the joined text content of a tool result.
    ///
    /// `raw_text` is the concatenation of all `Content::Text` blocks from the
    /// raw tool output. The returned [`CompressionResult`] carries the
    /// replacement blocks.
    async fn compress(&self, raw_text: &str, ctx: &CompressionContext<'_>) -> CompressionResult;

    /// Human-readable name for logging / diagnostics.
    fn name(&self) -> &'static str;
}
