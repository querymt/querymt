//! Duplicate code detection middleware
//!
//! This middleware automatically detects when newly written code is similar to
//! existing code in the codebase, and injects a warning message to help the LLM
//! self-correct and use existing functions instead of duplicating code.
//!
//! # Example (programmatic)
//!
//! ```ignore
//! use querymt_agent::middleware::DedupCheckMiddleware;
//!
//! let agent = Agent::single()
//!     .provider("anthropic", "claude-sonnet")
//!     .middleware(|agent| {
//!         DedupCheckMiddleware::new(agent.session_runtime())
//!             .threshold(0.8)
//!             .min_lines(5)
//!             .with_event_bus(agent.event_bus())
//!     })
//!     .build()
//!     .await?;
//! ```
//!
//! # Example (TOML config)
//!
//! ```toml
//! [[middleware]]
//! type = "dedup_check"
//! threshold = 0.8
//! min_lines = 5
//! ```

use crate::agent::core::QueryMTAgent;
use crate::event_bus::EventBus;
use crate::events::AgentEventKind;
use crate::index::{DiffPaths, FunctionIndex, IndexedFunctionEntry, SimilarFunctionMatch};
use crate::middleware::factory::MiddlewareFactory;
use crate::middleware::{ExecutionState, MiddlewareDriver, Result, ToolResult};
use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::instrument;

/// Location of a function in source code
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionLocation {
    pub file_path: PathBuf,
    pub function_name: String,
    pub start_line: u32,
    pub end_line: u32,
}

impl FunctionLocation {
    pub fn from_entry(entry: &IndexedFunctionEntry) -> Self {
        Self {
            file_path: entry.file_path.clone(),
            function_name: entry.name.clone(),
            start_line: entry.start_line,
            end_line: entry.end_line,
        }
    }
}

/// A warning about duplicate code
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateWarning {
    /// The newly written function that is similar to existing code
    pub new_function: FunctionLocation,
    /// Matching functions from the codebase, ordered by similarity (highest first)
    pub matches: Vec<SimilarMatch>,
}

/// A match found in the existing codebase
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarMatch {
    pub file_path: PathBuf,
    pub function_name: String,
    pub start_line: u32,
    pub end_line: u32,
    pub similarity: f64,
}

impl From<&SimilarFunctionMatch> for SimilarMatch {
    fn from(m: &SimilarFunctionMatch) -> Self {
        Self {
            file_path: m.function.file_path.clone(),
            function_name: m.function.name.clone(),
            start_line: m.function.start_line,
            end_line: m.function.end_line,
            similarity: m.similarity,
        }
    }
}

/// Middleware that checks for duplicate/similar code after file mutations
pub struct DedupCheckMiddleware {
    /// Whether duplicate detection is enabled
    enabled: bool,
    /// Similarity threshold for considering code as duplicate (0.0 - 1.0)
    threshold: f64,
    /// Minimum number of lines for a function to be analyzed
    min_lines: usize,
    /// Session runtime map for accessing function indexes
    session_runtime: Arc<Mutex<HashMap<String, Arc<crate::agent::core::SessionRuntime>>>>,
    /// Accumulated tool results from the current batch (reset each cycle)
    pending_results: Arc<Mutex<Vec<ToolResult>>>,
    /// Optional event bus for emitting duplicate detection events
    event_bus: Option<Arc<EventBus>>,
}

impl DedupCheckMiddleware {
    /// Create a new DedupCheckMiddleware with default settings
    ///
    /// Default settings:
    /// - enabled: true
    /// - threshold: 0.8 (80% similarity)
    /// - min_lines: 5
    pub fn new(
        session_runtime: Arc<Mutex<HashMap<String, Arc<crate::agent::core::SessionRuntime>>>>,
    ) -> Self {
        Self {
            enabled: true,
            threshold: 0.8,
            min_lines: 5,
            session_runtime,
            pending_results: Arc::new(Mutex::new(Vec::new())),
            event_bus: None,
        }
    }

    /// Set whether duplicate detection is enabled
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Set the similarity threshold (0.0 - 1.0)
    ///
    /// Functions with similarity above this threshold will be flagged as duplicates.
    /// Default is 0.8 (80%).
    pub fn threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// Set the minimum number of lines for a function to be analyzed
    ///
    /// Functions shorter than this will be ignored.
    /// Default is 5 lines.
    pub fn min_lines(mut self, min_lines: usize) -> Self {
        self.min_lines = min_lines;
        self
    }

    /// Set the event bus for emitting duplicate detection events
    pub fn with_event_bus(mut self, event_bus: Arc<EventBus>) -> Self {
        self.event_bus = Some(event_bus);
        self
    }

    /// Extract changed file paths from tool results
    fn extract_changed_paths(results: &[ToolResult]) -> DiffPaths {
        let mut combined = DiffPaths::default();

        for result in results {
            if let Some(ref snapshot) = result.snapshot_part
                && let Some(paths) = snapshot.changed_paths()
            {
                combined.added.extend(paths.added.iter().cloned());
                combined.modified.extend(paths.modified.iter().cloned());
                combined.removed.extend(paths.removed.iter().cloned());
            }
        }

        // Deduplicate paths
        combined.added.sort();
        combined.added.dedup();
        combined.modified.sort();
        combined.modified.dedup();
        combined.removed.sort();
        combined.removed.dedup();

        combined
    }

    /// Check for duplicate code in changed files
    #[instrument(
        name = "middleware.dedup_check.analyze",
        skip(self, function_index, changed_paths),
        fields(
            files_to_check = tracing::field::Empty,
            warnings_generated = tracing::field::Empty
        )
    )]
    async fn check_for_duplicates(
        &self,
        function_index: &RwLock<FunctionIndex>,
        changed_paths: &DiffPaths,
    ) -> Vec<DuplicateWarning> {
        let mut warnings = Vec::new();

        // Get all added and modified files
        let files_to_check: Vec<_> = changed_paths.changed_files().collect();

        // Record the number of files to check
        tracing::Span::current().record("files_to_check", files_to_check.len());

        if files_to_check.is_empty() {
            tracing::Span::current().record("warnings_generated", 0usize);
            return warnings;
        }

        // Read each file and check for duplicates
        let index = function_index.read().await;

        for file_path in files_to_check {
            // Read file content
            let source = match std::fs::read_to_string(file_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Find similar functions
            let results = index.find_similar_to_code(file_path, &source);

            for (entry, matches) in results {
                // Filter matches by threshold
                let filtered_matches: Vec<SimilarMatch> = matches
                    .iter()
                    .filter(|m| m.similarity >= self.threshold)
                    .map(SimilarMatch::from)
                    .collect();

                if !filtered_matches.is_empty() {
                    warnings.push(DuplicateWarning {
                        new_function: FunctionLocation::from_entry(&entry),
                        matches: filtered_matches,
                    });
                }
            }
        }

        // Record the number of warnings generated
        tracing::Span::current().record("warnings_generated", warnings.len());

        warnings
    }

    /// Format warnings into a human-readable message for LLM injection
    fn format_warning_message(warnings: &[DuplicateWarning]) -> String {
        if warnings.is_empty() {
            return String::new();
        }

        let mut message = String::from("\n⚠️ Similar code detected:\n\n");

        for warning in warnings {
            let new_func = &warning.new_function;
            message.push_str(&format!(
                "Function `{}` in {}:{}-{} is similar to:\n",
                new_func.function_name,
                new_func.file_path.display(),
                new_func.start_line,
                new_func.end_line
            ));

            for m in &warning.matches {
                message.push_str(&format!(
                    "  • `{}` in {}:{}-{} ({:.0}% similar)\n",
                    m.function_name,
                    m.file_path.display(),
                    m.start_line,
                    m.end_line,
                    m.similarity * 100.0
                ));
            }
            message.push('\n');
        }

        message.push_str("Consider using existing functions or extracting shared logic.\n");

        message
    }

    /// Update the function index with newly changed files
    #[instrument(
        name = "middleware.dedup_check.update_index",
        skip(self, function_index, changed_paths),
        fields(
            files_removed = %changed_paths.removed.len(),
            files_updated = tracing::field::Empty
        )
    )]
    async fn update_index(
        &self,
        function_index: &RwLock<FunctionIndex>,
        changed_paths: &DiffPaths,
    ) {
        let mut index = function_index.write().await;

        // Remove deleted files
        for path in &changed_paths.removed {
            index.remove_file(path);
        }

        // Update added/modified files
        let mut files_updated = 0usize;
        for path in changed_paths.changed_files() {
            if let Ok(source) = std::fs::read_to_string(path) {
                index.update_file(path, &source);
                files_updated += 1;
            }
        }

        tracing::Span::current().record("files_updated", files_updated);
    }
}

#[async_trait]
impl MiddlewareDriver for DedupCheckMiddleware {
    #[instrument(
        name = "middleware.dedup_check",
        skip(self, state),
        fields(
            input_state = %state.name(),
            output_state = tracing::field::Empty,
            files_checked = tracing::field::Empty,
            duplicates_found = tracing::field::Empty,
            warning_injected = tracing::field::Empty
        )
    )]
    async fn next_state(&self, state: ExecutionState) -> Result<ExecutionState> {
        if !self.enabled {
            tracing::Span::current().record("output_state", state.name());
            return Ok(state);
        }

        match state {
            // Collect tool results as they come through
            ExecutionState::AfterTool { ref result, .. } => {
                // Accumulate results
                let mut pending = self.pending_results.lock().await;
                pending.push((**result).clone());
                tracing::Span::current().record("output_state", state.name());
                Ok(state)
            }

            // When all tools are done and we're about to return to LLM, check for duplicates
            ExecutionState::ProcessingToolCalls {
                remaining_calls,
                results,
                context,
            } if remaining_calls.is_empty() => {
                // Get session ID from context
                let session_id = context.session_id.to_string();

                // Get function index from session runtime
                let function_index = {
                    let runtimes = self.session_runtime.lock().await;
                    runtimes
                        .get(&session_id)
                        .and_then(|r| r.function_index.get().cloned())
                };

                let Some(function_index) = function_index else {
                    // No function index available, skip dedup check
                    tracing::Span::current().record("output_state", "ProcessingToolCalls");
                    return Ok(ExecutionState::ProcessingToolCalls {
                        remaining_calls,
                        results,
                        context,
                    });
                };

                // Extract changed paths from all tool results
                let changed_paths = Self::extract_changed_paths(&results);

                if changed_paths.is_empty() {
                    // No file changes, nothing to check
                    tracing::Span::current().record("files_checked", 0usize);
                    tracing::Span::current().record("duplicates_found", 0usize);
                    tracing::Span::current().record("warning_injected", false);
                    tracing::Span::current().record("output_state", "ProcessingToolCalls");
                    return Ok(ExecutionState::ProcessingToolCalls {
                        remaining_calls,
                        results,
                        context,
                    });
                }

                // Record the number of files being checked
                let files_checked = changed_paths.added.len() + changed_paths.modified.len();
                tracing::Span::current().record("files_checked", files_checked);

                // Check for duplicates
                let warnings = self
                    .check_for_duplicates(&function_index, &changed_paths)
                    .await;

                // Record duplicates found
                tracing::Span::current().record("duplicates_found", warnings.len());

                // Update the index with new/modified functions
                self.update_index(&function_index, &changed_paths).await;

                // If duplicates found, inject warning message and emit event
                if !warnings.is_empty() {
                    let warning_message = Self::format_warning_message(&warnings);

                    log::info!(
                        "DedupCheckMiddleware: Found {} duplicate code warnings",
                        warnings.len()
                    );

                    // Emit event if event bus is available
                    if let Some(ref event_bus) = self.event_bus {
                        event_bus.publish(
                            &session_id,
                            AgentEventKind::DuplicateCodeDetected {
                                warnings: warnings.clone(),
                            },
                        );
                    }

                    // Inject the warning into the context
                    let new_context = Arc::new(context.inject_message(warning_message));

                    tracing::Span::current().record("warning_injected", true);
                    tracing::Span::current().record("output_state", "ProcessingToolCalls");
                    return Ok(ExecutionState::ProcessingToolCalls {
                        remaining_calls,
                        results,
                        context: new_context,
                    });
                }

                tracing::Span::current().record("warning_injected", false);
                tracing::Span::current().record("output_state", "ProcessingToolCalls");
                Ok(ExecutionState::ProcessingToolCalls {
                    remaining_calls,
                    results,
                    context,
                })
            }

            // Pass through all other states
            _ => {
                tracing::Span::current().record("output_state", state.name());
                Ok(state)
            }
        }
    }

    fn reset(&self) {
        // Clear accumulated results at the start of a new cycle
        if let Ok(mut pending) = self.pending_results.try_lock() {
            pending.clear();
        }
    }

    fn name(&self) -> &'static str {
        "DedupCheckMiddleware"
    }
}

/// Factory for creating DedupCheckMiddleware from config
pub struct DedupCheckFactory;

impl MiddlewareFactory for DedupCheckFactory {
    fn type_name(&self) -> &'static str {
        "dedup_check"
    }

    fn create(
        &self,
        config: &Value,
        agent: &QueryMTAgent,
    ) -> AnyhowResult<Arc<dyn MiddlewareDriver>> {
        // Check if explicitly disabled
        let enabled = config
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        if !enabled {
            return Err(anyhow::anyhow!("Middleware disabled"));
        }

        let mut mw =
            DedupCheckMiddleware::new(agent.session_runtime()).with_event_bus(agent.event_bus());

        if let Some(v) = config.get("threshold").and_then(|v| v.as_f64()) {
            mw = mw.threshold(v);
        }
        if let Some(v) = config.get("min_lines").and_then(|v| v.as_u64()) {
            mw = mw.min_lines(v as usize);
        }

        Ok(Arc::new(mw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MessagePart;

    #[test]
    fn test_middleware_defaults() {
        let session_runtime = Arc::new(Mutex::new(HashMap::new()));
        let mw = DedupCheckMiddleware::new(session_runtime);
        assert!(mw.enabled);
        assert!((mw.threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(mw.min_lines, 5);
    }

    #[test]
    fn test_middleware_builder() {
        let session_runtime = Arc::new(Mutex::new(HashMap::new()));
        let mw = DedupCheckMiddleware::new(session_runtime)
            .enabled(false)
            .threshold(0.9)
            .min_lines(10);

        assert!(!mw.enabled);
        assert!((mw.threshold - 0.9).abs() < f64::EPSILON);
        assert_eq!(mw.min_lines, 10);
    }

    #[test]
    fn test_format_warning_message_empty() {
        let message = DedupCheckMiddleware::format_warning_message(&[]);
        assert!(message.is_empty());
    }

    #[test]
    fn test_format_warning_message() {
        let warnings = vec![DuplicateWarning {
            new_function: FunctionLocation {
                file_path: PathBuf::from("src/utils.ts"),
                function_name: "calculateTotal".to_string(),
                start_line: 10,
                end_line: 20,
            },
            matches: vec![SimilarMatch {
                file_path: PathBuf::from("src/helpers.ts"),
                function_name: "computeSum".to_string(),
                start_line: 5,
                end_line: 15,
                similarity: 0.85,
            }],
        }];

        let message = DedupCheckMiddleware::format_warning_message(&warnings);

        assert!(message.contains("Similar code detected"));
        assert!(message.contains("calculateTotal"));
        assert!(message.contains("computeSum"));
        assert!(message.contains("85%"));
    }

    #[test]
    fn test_extract_changed_paths_empty() {
        let results: Vec<ToolResult> = vec![];
        let paths = DedupCheckMiddleware::extract_changed_paths(&results);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_extract_changed_paths() {
        let results = vec![ToolResult {
            call_id: "1".to_string(),
            content: "ok".to_string(),
            is_error: false,
            tool_name: Some("write_file".to_string()),
            tool_arguments: None,
            snapshot_part: Some(MessagePart::Snapshot {
                root_hash: crate::hash::RapidHash::new(b"test"),
                changed_paths: DiffPaths {
                    added: vec![PathBuf::from("new.ts")],
                    modified: vec![],
                    removed: vec![],
                },
            }),
        }];

        let paths = DedupCheckMiddleware::extract_changed_paths(&results);
        assert_eq!(paths.added.len(), 1);
        assert!(paths.added.contains(&PathBuf::from("new.ts")));
    }
}
