//! Minimal verification service for executing verification specs

use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::tools::{ToolContext, ToolRegistry};
use crate::verification::{
    Expectation, VerificationError, VerificationResult, VerificationSpec, VerificationStep,
    VerificationStrategy,
};

/// Context for verification execution
pub struct VerificationContext {
    pub session_id: String,
    pub task_id: Option<String>,
    pub delegation_id: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub tool_registry: Arc<ToolRegistry>,
}

/// Service for executing verification specifications
pub struct VerificationService {
    tool_registry: Arc<ToolRegistry>,
}

impl VerificationService {
    pub fn new(tool_registry: Arc<ToolRegistry>) -> Self {
        Self { tool_registry }
    }

    /// Execute a verification spec
    pub async fn verify(
        &self,
        spec: &VerificationSpec,
        context: &VerificationContext,
    ) -> VerificationResult {
        if spec.steps.is_empty() {
            eprintln!("No verification steps to execute");
            return Ok(());
        }

        eprintln!("Running verification: {}", spec.description);
        let start = Instant::now();

        let result = match &spec.strategy {
            VerificationStrategy::All => self.verify_all(&spec.steps, context).await,
            VerificationStrategy::Any => self.verify_any(&spec.steps, context).await,
            VerificationStrategy::UntilFailure => {
                self.verify_until_failure(&spec.steps, context).await
            }
        };

        match &result {
            Ok(_) => eprintln!(
                "Verification passed: {} ({}ms)",
                spec.description,
                start.elapsed().as_millis()
            ),
            Err(e) => eprintln!("Verification failed: {} - {}", spec.description, e),
        }

        result
    }

    async fn verify_all(
        &self,
        steps: &[VerificationStep],
        context: &VerificationContext,
    ) -> VerificationResult {
        for (idx, step) in steps.iter().enumerate() {
            self.verify_step(step, context)
                .await
                .map_err(|e| VerificationError::StepFailed {
                    step_index: idx,
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }

    async fn verify_any(
        &self,
        steps: &[VerificationStep],
        context: &VerificationContext,
    ) -> VerificationResult {
        let mut errors = Vec::new();

        for (idx, step) in steps.iter().enumerate() {
            match self.verify_step(step, context).await {
                Ok(_) => return Ok(()), // At least one passed
                Err(e) => {
                    errors.push(format!("Step {}: {}", idx, e));
                }
            }
        }

        Err(VerificationError::StepFailed {
            step_index: 0,
            message: format!("All verification steps failed: {}", errors.join("; ")),
        })
    }

    async fn verify_until_failure(
        &self,
        steps: &[VerificationStep],
        context: &VerificationContext,
    ) -> VerificationResult {
        for (idx, step) in steps.iter().enumerate() {
            self.verify_step(step, context)
                .await
                .map_err(|e| VerificationError::StepFailed {
                    step_index: idx,
                    message: e.to_string(),
                })?;
        }
        Ok(())
    }

    async fn verify_step(
        &self,
        step: &VerificationStep,
        context: &VerificationContext,
    ) -> VerificationResult {
        match step {
            VerificationStep::ToolCall {
                tool_name,
                arguments,
                expectation,
                error_message: _,
            } => {
                self.verify_tool_call(tool_name.as_ref(), arguments.clone(), expectation, context)
                    .await
            }

            VerificationStep::FileAssertion {
                path,
                exists,
                contains,
                matches_regex: _,
            } => self.verify_file(path, *exists, contains.as_deref()).await,

            VerificationStep::WaitFor { .. } => Err(VerificationError::StepFailed {
                step_index: 0,
                message: "WaitFor not yet implemented".to_string(),
            }),

            VerificationStep::Parallel { .. } => Err(VerificationError::StepFailed {
                step_index: 0,
                message: "Parallel not yet implemented".to_string(),
            }),
        }
    }

    async fn verify_tool_call(
        &self,
        tool_name: &str,
        arguments: Value,
        expectation: &Expectation,
        context: &VerificationContext,
    ) -> VerificationResult {
        // Find the tool
        let tool = self
            .tool_registry
            .find(tool_name)
            .ok_or_else(|| VerificationError::UnknownTool(tool_name.to_string()))?;

        log::debug!("Verifying with tool: {} {:?}", tool_name, arguments);

        // Create a tool context for verification
        let tool_context = VerificationToolContext {
            session_id: context.session_id.clone(),
            cwd: context.cwd.clone(),
        };

        // Execute the tool
        let result = tool.call(arguments, &tool_context).await.map_err(|e| {
            VerificationError::ToolExecutionFailed {
                tool_name: tool_name.to_string(),
                message: e.to_string(),
            }
        })?;

        // Parse result if it's JSON
        let result_value: Value = serde_json::from_str(&result).unwrap_or(Value::String(result));

        // Check expectation
        self.check_expectation(&result_value, expectation, tool_name)
    }

    fn check_expectation(
        &self,
        result: &Value,
        expectation: &Expectation,
        tool_name: &str,
    ) -> VerificationResult {
        match expectation {
            Expectation::Success => {
                // For shell tools, check exit_code
                if let Some(exit_code) = result.get("exit_code").and_then(|v| v.as_i64())
                    && exit_code != 0
                {
                    return Err(VerificationError::ToolExecutionFailed {
                        tool_name: tool_name.to_string(),
                        message: format!(
                            "Tool '{}' failed with exit code {}",
                            tool_name, exit_code
                        ),
                    });
                }
                Ok(())
            }

            Expectation::Contains(text) => {
                let output =
                    serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string());
                if !output.contains(text) {
                    return Err(VerificationError::StepFailed {
                        step_index: 0,
                        message: format!("Output does not contain expected text: '{}'", text),
                    });
                }
                Ok(())
            }

            Expectation::MatchesRegex(_regex) => Err(VerificationError::StepFailed {
                step_index: 0,
                message: "MatchesRegex not yet implemented".to_string(),
            }),

            Expectation::JsonMatches(expected) => {
                if result != expected {
                    return Err(VerificationError::StepFailed {
                        step_index: 0,
                        message: "JSON structure does not match expected".to_string(),
                    });
                }
                Ok(())
            }

            Expectation::Custom { expression, .. } => {
                eprintln!("Custom expectation not yet implemented: {}", expression);
                Ok(())
            }
        }
    }

    async fn verify_file(
        &self,
        path: &Path,
        should_exist: bool,
        contains: Option<&str>,
    ) -> VerificationResult {
        use std::fs;

        let exists = path.exists();

        if exists != should_exist {
            return Err(VerificationError::StepFailed {
                step_index: 0,
                message: format!(
                    "File '{}' existence check failed: expected {}, got {}",
                    path.display(),
                    should_exist,
                    exists
                ),
            });
        }

        if !exists {
            return Ok(());
        }

        // If file exists, check content
        if let Some(text) = contains {
            let content = fs::read_to_string(path)?;
            if !content.contains(text) {
                return Err(VerificationError::StepFailed {
                    step_index: 0,
                    message: format!(
                        "File '{}' does not contain expected text: '{}'",
                        path.display(),
                        text
                    ),
                });
            }
        }

        Ok(())
    }
}

/// ToolContext implementation for verification
struct VerificationToolContext {
    session_id: String,
    cwd: Option<std::path::PathBuf>,
}

#[async_trait::async_trait]
impl ToolContext for VerificationToolContext {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    async fn record_progress(
        &self,
        _kind: &str,
        _content: String,
        _metadata: Option<Value>,
    ) -> Result<String, crate::tools::ToolError> {
        Ok("".to_string())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
