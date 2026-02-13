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
//!         DedupCheckMiddleware::new()
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

use crate::event_bus::EventBus;
use crate::events::AgentEventKind;
use crate::index::{DiffPaths, FunctionIndex, IndexedFunctionEntry, SimilarFunctionMatch};
use crate::middleware::factory::MiddlewareFactory;
use crate::middleware::{
    ConversationContext, ExecutionState, MiddlewareDriver, Result, ToolResult,
};
use anyhow::Result as AnyhowResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub body_text: String,
}

impl From<&SimilarFunctionMatch> for SimilarMatch {
    fn from(m: &SimilarFunctionMatch) -> Self {
        Self {
            file_path: m.function.file_path.clone(),
            function_name: m.function.name.clone(),
            start_line: m.function.start_line,
            end_line: m.function.end_line,
            similarity: m.similarity,
            body_text: m.function.body_text.clone(),
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
    /// Accumulated tool results from the current batch (reset each cycle)
    pending_results: Arc<Mutex<Vec<ToolResult>>>,
    /// Optional event bus for emitting duplicate detection events
    event_bus: Option<Arc<EventBus>>,
    /// Guard to prevent multiple reviews in the same turn
    already_reviewed_this_turn: AtomicBool,
    /// Last context seen, used for building BeforeLlmCall state in on_turn_end
    last_context: Arc<Mutex<Option<Arc<ConversationContext>>>>,
}

impl Default for DedupCheckMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

impl DedupCheckMiddleware {
    /// Create a new DedupCheckMiddleware with default settings
    ///
    /// Default settings:
    /// - enabled: true
    /// - threshold: 0.8 (80% similarity)
    /// - min_lines: 5
    pub fn new() -> Self {
        Self {
            enabled: true,
            threshold: 0.8,
            min_lines: 5,
            pending_results: Arc::new(Mutex::new(Vec::new())),
            event_bus: None,
            already_reviewed_this_turn: AtomicBool::new(false),
            last_context: Arc::new(Mutex::new(None)),
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

    /// Extract changed file paths from tool results (for testing)
    #[cfg(test)]
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

    /// Format warnings into a human-readable review message for LLM injection
    fn format_review_message(warnings: &[DuplicateWarning]) -> String {
        if warnings.is_empty() {
            return String::new();
        }

        let mut message = String::from(
            "\n📋 POST-TURN CODE REVIEW: Potential code duplication found.\n\n\
             The following functions you wrote appear similar to existing code in the codebase.\n\n",
        );

        for warning in warnings {
            let new_func = &warning.new_function;
            message.push_str(&format!(
                "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
                 📝 Function: `{}` in {}:{}-{}\n\n",
                new_func.function_name,
                new_func.file_path.display(),
                new_func.start_line,
                new_func.end_line
            ));

            message.push_str("Similar to:\n\n");

            for m in &warning.matches {
                message.push_str(&format!(
                    "  `{}` in {} (lines {}-{}, {:.0}% similar)\n",
                    m.function_name,
                    m.file_path.display(),
                    m.start_line,
                    m.end_line,
                    m.similarity * 100.0
                ));

                // Show the existing function's source code (truncated if too long)
                let code_preview = if m.body_text.lines().count() > 30 {
                    let preview: String =
                        m.body_text.lines().take(25).collect::<Vec<_>>().join("\n");
                    format!(
                        "{}...\n  // ... truncated ({} total lines)",
                        preview,
                        m.body_text.lines().count()
                    )
                } else {
                    m.body_text.clone()
                };

                message.push_str(&format!("  ```\n{}\n  ```\n\n", code_preview));
            }
        }

        message.push_str(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
             REVIEW NOTES:\n\
             - If you intentionally moved/extracted this function as part of a refactor, \
               make sure the original is removed from the source file.\n\
             - If this is unintentional duplication, consider using the existing function instead.\n\
             - If the functions serve different purposes despite similar structure, no action needed.\n",
        );

        message
    }

    /// Filter out functions that were moved (appear in both removed and added/modified paths)
    fn filter_moved_functions(
        &self,
        warnings: Vec<DuplicateWarning>,
        changed_paths: &DiffPaths,
    ) -> Vec<DuplicateWarning> {
        warnings
            .into_iter()
            .filter(|warning| {
                // Keep warning only if the matched function is NOT in a file that was
                // removed/modified during this turn (which would indicate a move)
                warning.matches.iter().any(|m| {
                    // If the similar match's file was also changed in this turn,
                    // the function may have been moved/removed — not a real duplicate
                    !changed_paths.removed.contains(&m.file_path)
                })
            })
            .map(|mut warning| {
                // Also filter individual matches
                warning
                    .matches
                    .retain(|m| !changed_paths.removed.contains(&m.file_path));
                warning
            })
            .filter(|w| !w.matches.is_empty())
            .collect()
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
    /// Capture the latest context for use in on_turn_end
    async fn on_after_llm(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        // Capture the latest context for use in on_turn_end
        if let Some(ctx) = state.context() {
            *self.last_context.lock().await = Some(ctx.clone());
        }
        Ok(state)
    }

    #[instrument(
        name = "middleware.dedup_check.turn_end",
        skip(self, state, runtime),
        fields(
            input_state = %state.name(),
            output_state = tracing::field::Empty,
            files_checked = tracing::field::Empty,
            duplicates_found = tracing::field::Empty,
            review_injected = tracing::field::Empty
        )
    )]
    async fn on_turn_end(
        &self,
        state: ExecutionState,
        runtime: Option<&Arc<crate::agent::core::SessionRuntime>>,
    ) -> Result<ExecutionState> {
        if !self.enabled {
            tracing::Span::current().record("output_state", state.name());
            return Ok(state);
        }

        // Only run on Complete state
        if !matches!(state, ExecutionState::Complete) {
            tracing::Span::current().record("output_state", state.name());
            return Ok(state);
        }

        // Guard: only review once per turn
        if self.already_reviewed_this_turn.swap(true, Ordering::SeqCst) {
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        }

        // Get the last context for building BeforeLlmCall state
        let last_context = self.last_context.lock().await.clone();
        let Some(context) = last_context else {
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        };

        // Get function_index and turn_diffs from the runtime parameter
        let Some(runtime) = runtime else {
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        };

        let function_index = runtime.function_index.get().cloned();
        let Some(function_index) = function_index else {
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        };

        // Get and clear turn_diffs
        let turn_diffs = runtime.turn_diffs.lock().ok().map(|mut diffs| {
            let accumulated = diffs.clone();
            *diffs = DiffPaths::default();
            accumulated
        });

        let Some(turn_diffs) = turn_diffs else {
            tracing::Span::current().record("files_checked", 0usize);
            tracing::Span::current().record("duplicates_found", 0usize);
            tracing::Span::current().record("review_injected", false);
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        };

        if turn_diffs.is_empty() {
            tracing::Span::current().record("files_checked", 0usize);
            tracing::Span::current().record("duplicates_found", 0usize);
            tracing::Span::current().record("review_injected", false);
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        }

        // Record the number of files being checked
        let files_checked = turn_diffs.added.len() + turn_diffs.modified.len();
        tracing::Span::current().record("files_checked", files_checked);

        // Check for duplicates
        let mut warnings = self
            .check_for_duplicates(&function_index, &turn_diffs)
            .await;

        // Filter out moved functions
        warnings = self.filter_moved_functions(warnings, &turn_diffs);

        // Record duplicates found
        tracing::Span::current().record("duplicates_found", warnings.len());

        // Update the index with new/modified functions
        self.update_index(&function_index, &turn_diffs).await;

        // If duplicates found after filtering, inject review message
        if !warnings.is_empty() {
            let review_message = Self::format_review_message(&warnings);

            log::info!(
                "DedupCheckMiddleware: Found {} duplicate code warnings in turn review",
                warnings.len()
            );

            // Emit event if event bus is available
            if let Some(ref event_bus) = self.event_bus {
                event_bus.publish(
                    context.session_id.as_ref(),
                    AgentEventKind::DuplicateCodeDetected {
                        warnings: warnings.clone(),
                    },
                );
            }

            // Inject the review into a new context
            let new_context = Arc::new(context.inject_message(review_message));

            tracing::Span::current().record("review_injected", true);
            tracing::Span::current().record("output_state", "BeforeLlmCall");
            return Ok(ExecutionState::BeforeLlmCall {
                context: new_context,
            });
        }

        tracing::Span::current().record("review_injected", false);
        tracing::Span::current().record("output_state", "Complete");
        Ok(state)
    }

    fn reset(&self) {
        // Reset review guard and clear accumulated results
        self.already_reviewed_this_turn
            .store(false, Ordering::SeqCst);
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
        agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> AnyhowResult<Arc<dyn MiddlewareDriver>> {
        // Check if explicitly disabled
        let enabled = config
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        if !enabled {
            return Err(anyhow::anyhow!("Middleware disabled"));
        }

        let mut mw = DedupCheckMiddleware::new().with_event_bus(agent_config.event_bus.clone());

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
        let mw = DedupCheckMiddleware::new();
        assert!(mw.enabled);
        assert!((mw.threshold - 0.8).abs() < f64::EPSILON);
        assert_eq!(mw.min_lines, 5);
    }

    #[test]
    fn test_middleware_builder() {
        let mw = DedupCheckMiddleware::new()
            .enabled(false)
            .threshold(0.9)
            .min_lines(10);

        assert!(!mw.enabled);
        assert!((mw.threshold - 0.9).abs() < f64::EPSILON);
        assert_eq!(mw.min_lines, 10);
    }

    #[test]
    fn test_format_review_message_empty() {
        let message = DedupCheckMiddleware::format_review_message(&[]);
        assert!(message.is_empty());
    }

    #[test]
    fn test_format_review_message() {
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
                body_text: "function computeSum(items: number[]): number {\n  return items.reduce((a, b) => a + b, 0);\n}".to_string(),
            }],
        }];

        let message = DedupCheckMiddleware::format_review_message(&warnings);

        assert!(message.contains("POST-TURN CODE REVIEW"));
        assert!(message.contains("calculateTotal"));
        assert!(message.contains("computeSum"));
        assert!(message.contains("85%"));
        assert!(message.contains("REVIEW NOTES"));
        assert!(message.contains("function computeSum")); // Verify source code is included
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
