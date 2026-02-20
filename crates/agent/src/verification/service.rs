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

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use crate::verification::{
        Expectation, VerificationSpec, VerificationStep, VerificationStrategy,
    };
    use std::borrow::Cow;
    use std::path::PathBuf;

    fn make_context(cwd: Option<PathBuf>) -> VerificationContext {
        VerificationContext {
            session_id: "test-session".to_string(),
            task_id: None,
            delegation_id: None,
            cwd,
            tool_registry: Arc::new(ToolRegistry::new()),
        }
    }

    fn make_service() -> VerificationService {
        VerificationService::new(Arc::new(ToolRegistry::new()))
    }

    // ── VerificationContext construction ────────────────────────────────────

    #[test]
    fn test_context_construction() {
        let ctx = make_context(None);
        assert_eq!(ctx.session_id, "test-session");
        assert!(ctx.task_id.is_none());
        assert!(ctx.delegation_id.is_none());
        assert!(ctx.cwd.is_none());
    }

    #[test]
    fn test_context_with_all_fields() {
        let ctx = VerificationContext {
            session_id: "s1".to_string(),
            task_id: Some("t1".to_string()),
            delegation_id: Some("d1".to_string()),
            cwd: Some(PathBuf::from("/workspace")),
            tool_registry: Arc::new(ToolRegistry::new()),
        };
        assert_eq!(ctx.task_id.as_deref(), Some("t1"));
        assert_eq!(ctx.delegation_id.as_deref(), Some("d1"));
        assert_eq!(
            ctx.cwd.as_deref(),
            Some(PathBuf::from("/workspace").as_path())
        );
    }

    // ── VerificationService — empty spec ────────────────────────────────────

    #[tokio::test]
    async fn test_verify_empty_spec_returns_ok() {
        let service = make_service();
        let spec = VerificationSpec {
            description: "empty".to_string(),
            steps: vec![],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(None);
        let result = service.verify(&spec, &ctx).await;
        assert!(result.is_ok(), "empty spec should always pass");
    }

    // ── VerificationService — unknown tool ───────────────────────────────────

    #[tokio::test]
    async fn test_verify_unknown_tool_fails() {
        let service = make_service();
        let spec = VerificationSpec {
            description: "tool test".to_string(),
            steps: vec![VerificationStep::ToolCall {
                tool_name: Cow::Borrowed("nonexistent_tool"),
                arguments: serde_json::json!({}),
                expectation: Expectation::Success,
                error_message: None,
            }],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(None);
        let result = service.verify(&spec, &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        // verify_all wraps all inner errors as StepFailed; the message contains the unknown tool name
        assert!(
            matches!(
                err,
                crate::verification::VerificationError::StepFailed { .. }
            ),
            "expected StepFailed, got: {:?}",
            err
        );
        assert!(
            err.to_string().contains("nonexistent_tool") || err.to_string().contains("Unknown"),
            "expected error to mention the tool, got: {:?}",
            err
        );
    }

    // ── VerificationService — file assertions ────────────────────────────────

    #[tokio::test]
    async fn test_file_assertion_missing_file_when_expected_absent_passes() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let non_existent = tmp.path().join("does-not-exist.txt");

        let spec = VerificationSpec {
            description: "file absent".to_string(),
            steps: vec![VerificationStep::FileAssertion {
                path: non_existent,
                exists: false,
                contains: None,
                matches_regex: None,
            }],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(
            result.is_ok(),
            "file absent check should pass when file missing"
        );
    }

    #[tokio::test]
    async fn test_file_assertion_existing_file_passes() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("hello.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let spec = VerificationSpec {
            description: "file exists".to_string(),
            steps: vec![VerificationStep::FileAssertion {
                path: file_path.clone(),
                exists: true,
                contains: None,
                matches_regex: None,
            }],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(result.is_ok(), "existing file should pass existence check");
    }

    #[tokio::test]
    async fn test_file_assertion_content_check_passes() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("readme.md");
        std::fs::write(&file_path, "Hello, world!\nThis is a test.").unwrap();

        let spec = VerificationSpec {
            description: "content check".to_string(),
            steps: vec![VerificationStep::FileAssertion {
                path: file_path,
                exists: true,
                contains: Some("Hello, world!".to_string()),
                matches_regex: None,
            }],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_file_assertion_content_not_found_fails() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let file_path = tmp.path().join("readme.md");
        std::fs::write(&file_path, "nothing here").unwrap();

        let spec = VerificationSpec {
            description: "content missing".to_string(),
            steps: vec![VerificationStep::FileAssertion {
                path: file_path,
                exists: true,
                contains: Some("expected text not present".to_string()),
                matches_regex: None,
            }],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_file_assertion_file_missing_when_expected_exists_fails() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let absent = tmp.path().join("missing.txt");

        let spec = VerificationSpec {
            description: "expected to exist".to_string(),
            steps: vec![VerificationStep::FileAssertion {
                path: absent,
                exists: true,
                contains: None,
                matches_regex: None,
            }],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(result.is_err());
    }

    // ── Strategy tests ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_strategy_any_one_passing_step_succeeds() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let exists = tmp.path().join("exists.txt");
        std::fs::write(&exists, "data").unwrap();
        let absent = tmp.path().join("absent.txt");

        // Step 0 checks for absent file (expected to exist → fails)
        // Step 1 checks for existing file (expected to exist → passes)
        // Strategy::Any → overall pass
        let spec = VerificationSpec {
            description: "any strategy".to_string(),
            steps: vec![
                VerificationStep::FileAssertion {
                    path: absent,
                    exists: true, // will fail
                    contains: None,
                    matches_regex: None,
                },
                VerificationStep::FileAssertion {
                    path: exists,
                    exists: true, // will pass
                    contains: None,
                    matches_regex: None,
                },
            ],
            strategy: VerificationStrategy::Any,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(
            result.is_ok(),
            "Any strategy should pass when at least one step passes"
        );
    }

    #[tokio::test]
    async fn test_strategy_any_all_failing_returns_error() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let absent1 = tmp.path().join("a.txt");
        let absent2 = tmp.path().join("b.txt");

        let spec = VerificationSpec {
            description: "all fail".to_string(),
            steps: vec![
                VerificationStep::FileAssertion {
                    path: absent1,
                    exists: true,
                    contains: None,
                    matches_regex: None,
                },
                VerificationStep::FileAssertion {
                    path: absent2,
                    exists: true,
                    contains: None,
                    matches_regex: None,
                },
            ],
            strategy: VerificationStrategy::Any,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(
            result.is_err(),
            "Any strategy should fail when all steps fail"
        );
    }

    #[tokio::test]
    async fn test_strategy_until_failure_stops_at_first_error() {
        let service = make_service();
        let tmp = tempfile::TempDir::new().unwrap();
        let exists = tmp.path().join("real.txt");
        std::fs::write(&exists, "ok").unwrap();
        let absent = tmp.path().join("missing.txt");

        // Step 0 passes, step 1 fails → UntilFailure stops at step 1
        let spec = VerificationSpec {
            description: "until failure".to_string(),
            steps: vec![
                VerificationStep::FileAssertion {
                    path: exists,
                    exists: true,
                    contains: None,
                    matches_regex: None,
                },
                VerificationStep::FileAssertion {
                    path: absent,
                    exists: true, // will fail
                    contains: None,
                    matches_regex: None,
                },
            ],
            strategy: VerificationStrategy::UntilFailure,
        };
        let ctx = make_context(Some(tmp.path().to_path_buf()));
        let result = service.verify(&spec, &ctx).await;
        assert!(
            result.is_err(),
            "UntilFailure should fail when a step fails"
        );
    }

    // ── check_expectation (via verify_step → file assertions) ────────────────

    #[tokio::test]
    async fn test_wait_for_returns_not_implemented() {
        let service = make_service();
        let spec = VerificationSpec {
            description: "wait for".to_string(),
            steps: vec![VerificationStep::WaitFor {
                poll_interval_ms: 100,
                timeout_ms: 500,
                condition: Box::new(VerificationStep::FileAssertion {
                    path: PathBuf::from("/tmp/fake"),
                    exists: true,
                    contains: None,
                    matches_regex: None,
                }),
            }],
            strategy: VerificationStrategy::All,
        };
        let ctx = make_context(None);
        let result = service.verify(&spec, &ctx).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not yet implemented"),
            "expected 'not yet implemented', got: {}",
            msg
        );
    }
}
