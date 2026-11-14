//! Verification framework for delegations and tasks
//!
//! This module provides a structured way to verify that delegated tasks
//! have been completed successfully by executing tools and checking expectations.

pub mod service;

pub use crate::core_staging::verification::{
    Expectation, VerificationSpec, VerificationStep, VerificationStrategy,
};
use serde_json::Value;

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
    use std::borrow::Cow;

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
