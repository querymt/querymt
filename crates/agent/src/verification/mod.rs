//! Verification framework for delegations and tasks
//!
//! This module provides a structured way to verify that delegated tasks
//! have been completed successfully by executing tools and checking expectations.

pub mod service;

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
        /// Tool name (e.g., "shell", "read_file", "web_fetch")
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

/// Verification errors
#[derive(Debug, thiserror::Error)]
pub enum VerificationError {
    #[error("Verification step {step_index} failed: {message}")]
    StepFailed { step_index: usize, message: String },

    #[error("Tool execution failed: {tool_name} - {message}")]
    ToolExecutionFailed { tool_name: String, message: String },

    #[error("Unknown tool: {0}")]
    UnknownTool(String),

    #[error("Expectation failed: {expectation}")]
    ExpectationFailed {
        expectation: String,
        output: Value,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },

    #[error("Timeout after {0}ms")]
    Timeout(u64),

    #[error("File assertion failed: {0}")]
    FileAssertionFailed(String),

    #[error("Invalid regex: {0}")]
    InvalidRegex(String),

    #[error("I/O error: {0}")]
    Io(std::io::Error),
}

impl From<std::io::Error> for VerificationError {
    fn from(err: std::io::Error) -> Self {
        VerificationError::Io(err)
    }
}

/// Result type for verification
pub type VerificationResult = Result<(), VerificationError>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_verification_step_creation() {
        let step = VerificationStep::ToolCall {
            tool_name: Cow::Borrowed("shell"),
            arguments: json!({"command": "cargo check"}),
            expectation: Expectation::Success,
            error_message: None,
        };

        if let VerificationStep::ToolCall { tool_name, .. } = &step {
            assert_eq!(tool_name, "shell");
        } else {
            panic!("Expected ToolCall variant");
        }
    }

    #[test]
    fn test_cow_serialization() {
        let step = VerificationStep::ToolCall {
            tool_name: Cow::Borrowed("shell"),
            arguments: json!({"command": "test"}),
            expectation: Expectation::Success,
            error_message: None,
        };

        let serialized = serde_json::to_string(&step).unwrap();
        assert!(serialized.contains("shell"));

        let deserialized: VerificationStep = serde_json::from_str(&serialized).unwrap();
        if let VerificationStep::ToolCall { tool_name, .. } = deserialized {
            assert_eq!(tool_name, "shell");
        }
    }
}
