//! Verification specs: what to verify after a delegated task, and how.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::path::PathBuf;

/// Defines what and how to verify
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationSpec {
    /// Human-readable description (for logging/UI)
    pub description: String,

    /// The verification steps to execute
    pub steps: Vec<VerificationStep>,

    /// How to combine results (default: all must pass)
    #[serde(default)]
    pub strategy: VerificationStrategy,
}

/// A single verification step
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VerificationStep {
    /// Execute a tool call and verify the result
    ToolCall {
        /// Tool name (e.g., "shell", "read_tool", "web_fetch")
        tool_name: Cow<'static, str>,

        /// Arguments to pass to the tool (JSON)
        arguments: Value,

        /// How to verify the tool result
        expectation: Expectation,

        /// Optional: custom error message if this fails
        #[serde(default)]
        error_message: Option<String>,
    },

    /// Verify a file matches expectations
    FileAssertion {
        path: PathBuf,
        #[serde(default)]
        exists: bool,
        #[serde(default)]
        contains: Option<String>,
        #[serde(default)]
        matches_regex: Option<String>,
    },

    /// Wait for a condition (polling)
    WaitFor {
        #[serde(default = "default_poll_interval_ms")]
        poll_interval_ms: u64,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
        condition: Box<VerificationStep>,
    },

    /// Run multiple verifications in parallel
    Parallel { steps: Vec<VerificationStep> },
}

// Helper functions removed - tool names should be provided as strings directly
/// How to verify the result of a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Expectation {
    /// Tool call succeeded (exit code 0 for shell, no error for others)
    Success,

    /// Tool output contains this text (string search in JSON or string output)
    Contains(String),

    /// Tool output matches this regex
    MatchesRegex(String),

    /// Tool output is valid JSON matching this structure (exact match)
    JsonMatches(Value),

    /// Custom verification - placeholder for future extension
    Custom {
        expression: String,
        #[serde(default)]
        context: Value,
    },
}

/// Strategy for combining multiple verification steps
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum VerificationStrategy {
    /// All steps must pass (default)
    #[default]
    All,
    /// At least one step must pass
    Any,
    /// Steps are executed until one fails
    UntilFailure,
}

/// Default poll interval for WaitFor: 1 second
fn default_poll_interval_ms() -> u64 {
    1000
}

/// Default timeout for WaitFor: 30 seconds
fn default_timeout_ms() -> u64 {
    30000
}
