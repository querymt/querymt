//! Tool context and error types for unified tool interface

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Capability requirements that tools may need
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CapabilityRequirement {
    /// Requires filesystem access (cwd must be set)
    Filesystem,
}

/// Unified error type for all tools
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("Provider error: {0}")]
    ProviderError(String),
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
    #[error("Session error: {0}")]
    SessionError(String),
    #[error("Other error: {0}")]
    Other(#[from] anyhow::Error),
}

/// Core context trait that all tools receive during execution
#[async_trait]
pub trait ToolContext: Send + Sync {
    /// Get the current session ID
    fn session_id(&self) -> &str;

    /// Get the current working directory, if set.
    fn cwd(&self) -> Option<&Path>;

    /// Resolve a path. Returns error if cwd is None and path is relative.
    fn resolve_path(&self, path: &str) -> Result<PathBuf, ToolError> {
        let path = Path::new(path);
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            self.cwd().map(|cwd| cwd.join(path)).ok_or_else(|| {
                ToolError::InvalidRequest(
                    "Cannot resolve relative path: no working directory set".into(),
                )
            })
        }
    }

    /// Get access to the agent registry for delegation tools
    fn agent_registry(&self) -> Option<Arc<dyn crate::delegation::AgentRegistry>> {
        None
    }

    /// Record progress for long-running operations
    async fn record_progress(
        &self,
        kind: &str,
        content: String,
        metadata: Option<serde_json::Value>,
    ) -> Result<String, ToolError>;

    /// Access to tool-specific context extensions
    fn as_any(&self) -> &dyn Any;

    /// Ask the user a structured question and wait for a response.
    /// Returns the selected answer labels, or an error if the question cannot be delivered.
    ///
    /// # Arguments
    /// * `question_id` - Unique identifier for this question
    /// * `question` - The question text to display
    /// * `header` - Short header label (max 12 chars)
    /// * `options` - Available choices as (label, description) pairs
    /// * `multiple` - Whether multiple selections are allowed
    ///
    /// # Returns
    /// A vector of selected answer labels (option labels chosen by the user)
    ///
    /// # Default Implementation
    /// Falls back to stdin/stdout for CLI mode when no question channel is available.
    async fn ask_question(
        &self,
        _question_id: &str,
        question: &str,
        header: &str,
        options: &[(String, String)],
        multiple: bool,
    ) -> Result<Vec<String>, ToolError> {
        // Fallback to stdin/stdout for CLI mode
        use std::io::{self, Write};

        println!("\n{}", "=".repeat(60));
        println!("{}", header);
        println!("{}", "=".repeat(60));
        println!("{}\n", question);

        for (idx, (label, description)) in options.iter().enumerate() {
            println!("{}. {} - {}", idx + 1, label, description);
        }

        if multiple {
            println!(
                "\nEnter your choices (comma-separated numbers, or 'other' for custom input): "
            );
        } else {
            println!("\nEnter your choice (number, or 'other' for custom input): ");
        }

        print!("> ");
        io::stdout()
            .flush()
            .map_err(|e| ToolError::Other(e.into()))?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| ToolError::Other(e.into()))?;
        let input = input.trim();

        if input.to_lowercase() == "other" {
            println!("Enter your custom response: ");
            print!("> ");
            io::stdout()
                .flush()
                .map_err(|e| ToolError::Other(e.into()))?;

            let mut custom = String::new();
            io::stdin()
                .read_line(&mut custom)
                .map_err(|e| ToolError::Other(e.into()))?;
            return Ok(vec![custom.trim().to_string()]);
        }

        let selections: Vec<usize> = input
            .split(',')
            .filter_map(|s| s.trim().parse::<usize>().ok())
            .collect();

        let mut answers = Vec::new();
        for sel in selections {
            if sel > 0 && sel <= options.len() {
                answers.push(options[sel - 1].0.clone());
            }
        }

        if answers.is_empty() {
            Err(ToolError::InvalidRequest(
                "No valid selections made".to_string(),
            ))
        } else {
            Ok(answers)
        }
    }
}

/// Unified tool trait that replaces BuiltInTool
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool name
    fn name(&self) -> &str;

    /// Get the tool definition (schema, description, etc.)
    fn definition(&self) -> querymt::chat::Tool;

    /// Capabilities this tool requires. Default: empty.
    fn required_capabilities(&self) -> &'static [CapabilityRequirement] {
        &[]
    }

    /// Execute the tool with given arguments and context
    async fn call(
        &self,
        args: serde_json::Value,
        context: &dyn ToolContext,
    ) -> Result<String, ToolError>;

    /// Lifecycle hook: called when tool is initialized (optional)
    async fn initialize(&mut self, _context: &dyn ToolContext) -> Result<(), ToolError> {
        Ok(())
    }

    /// Lifecycle hook: called when tool is cleaned up (optional)
    async fn cleanup(&mut self) -> Result<(), ToolError> {
        Ok(())
    }
}

/// Conversion from old LLMError to ToolError for migration
impl From<querymt::error::LLMError> for ToolError {
    fn from(error: querymt::error::LLMError) -> Self {
        match error {
            querymt::error::LLMError::InvalidRequest(msg) => ToolError::InvalidRequest(msg),
            querymt::error::LLMError::ProviderError(msg) => ToolError::ProviderError(msg),
            _ => ToolError::ProviderError(error.to_string()),
        }
    }
}
