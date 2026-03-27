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
//!             .with_event_sink(agent.event_sink())
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

use crate::event_sink::EventSink;
use crate::events::AgentEventKind;
use crate::index::workspace_actor::{FindSimilarToCode, RemoveFile, UpdateFile};
use crate::index::{DiffPaths, IndexedFunctionEntry, SimilarFunctionMatch, WorkspaceHandle};
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
use tokio::sync::Mutex;
use tracing::instrument;
use typeshare::typeshare;

/// Maximum number of warnings shown inline in the injected summary message.
/// Remaining warnings are still in the overflow file but not listed in context.
const MAX_WARNINGS_IN_SUMMARY: usize = 50;

/// Maximum lines of code preview included per record in the overflow file.
const MAX_CODE_PREVIEW_LINES: usize = 20;

/// Maximum number of match references kept per warning in the compacted event payload.
const MAX_MATCHES_IN_EVENT: usize = 3;

/// Tracks where a warning record starts in the overflow file (1-indexed).
#[derive(Debug, Clone)]
struct WarningOffset {
    /// 1-indexed line number where this record starts in the overflow file
    line: usize,
    /// Number of lines this record occupies (including trailing blank line)
    length: usize,
}

/// Turn-identity token used to prevent duplicate review in the same prompt generation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewToken {
    session_id: String,
    generation: u64,
}

/// Location of a function in source code
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionLocation {
    #[typeshare(serialized_as = "string")]
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
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateWarning {
    /// The newly written function that is similar to existing code
    pub new_function: FunctionLocation,
    /// Matching functions from the codebase, ordered by similarity (highest first)
    pub matches: Vec<SimilarMatch>,
}

/// A match found in the existing codebase
#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarMatch {
    #[typeshare(serialized_as = "string")]
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
    /// Optional event sink for emitting duplicate detection events
    event_sink: Option<Arc<EventSink>>,
    /// Guard to prevent multiple reviews within the same prompt generation.
    /// If reset() is not called, this provides fallback robustness via generation scoping.
    last_reviewed: Arc<Mutex<Option<ReviewToken>>>,
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
            event_sink: None,
            last_reviewed: Arc::new(Mutex::new(None)),
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

    /// Set the event sink for emitting duplicate detection events
    pub fn with_event_sink(mut self, event_sink: Arc<EventSink>) -> Self {
        self.event_sink = Some(event_sink);
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
        skip(self, workspace, changed_paths),
        fields(
            files_to_check = tracing::field::Empty,
            warnings_generated = tracing::field::Empty
        )
    )]
    async fn check_for_duplicates(
        &self,
        workspace: &WorkspaceHandle,
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

        log::debug!(
            "DedupCheckMiddleware: checking {} file(s) for duplicates: {:?}",
            files_to_check.len(),
            files_to_check
        );

        for file_path in files_to_check {
            // Read file content
            let source = match std::fs::read_to_string(file_path) {
                Ok(s) => s,
                Err(e) => {
                    log::debug!(
                        "DedupCheckMiddleware: failed to read {:?}: {}",
                        file_path,
                        e
                    );
                    continue;
                }
            };

            // Find similar functions via actor message
            let results = workspace
                .actor
                .ask(FindSimilarToCode {
                    file_path: file_path.to_path_buf(),
                    source,
                })
                .await
                .unwrap_or_default();

            log::debug!(
                "DedupCheckMiddleware: {:?} — {} function(s) with candidate matches",
                file_path,
                results.len()
            );

            for (entry, matches) in results {
                log::debug!(
                    "DedupCheckMiddleware: function '{}' ({:?}:{}-{}) has {} candidate(s) \
                    (threshold={:.3})",
                    entry.name,
                    entry.file_path,
                    entry.start_line,
                    entry.end_line,
                    matches.len(),
                    self.threshold
                );

                // Filter matches by threshold
                let mut filtered_matches: Vec<SimilarMatch> = Vec::new();
                for m in &matches {
                    log::debug!(
                        "DedupCheckMiddleware:   candidate '{}' in {:?}:{}-{} similarity={:.4} \
                        ({})",
                        m.function.name,
                        m.function.file_path,
                        m.function.start_line,
                        m.function.end_line,
                        m.similarity,
                        if m.similarity >= self.threshold {
                            "PASS"
                        } else {
                            "below threshold"
                        }
                    );
                    if m.similarity >= self.threshold {
                        filtered_matches.push(SimilarMatch::from(m));
                    }
                }

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

    /// Return a workspace-relative version of `path` if possible, else the full path string.
    fn rel_path(path: &std::path::Path, workspace_root: Option<&std::path::Path>) -> String {
        if let Some(root) = workspace_root
            && let Ok(rel) = path.strip_prefix(root)
        {
            return rel.display().to_string();
        }
        path.display().to_string()
    }

    /// Build a compacted copy of `warnings` suitable for the event payload:
    /// - `body_text` stripped (empty string) to avoid bloating the DB/event bus
    /// - matches capped to `MAX_MATCHES_IN_EVENT` (highest similarity first)
    /// - paths kept as-is (not relativised — callers may not have a root)
    fn compact_warnings(warnings: &[DuplicateWarning]) -> Vec<DuplicateWarning> {
        warnings
            .iter()
            .map(|w| {
                let mut matches: Vec<SimilarMatch> = w
                    .matches
                    .iter()
                    .take(MAX_MATCHES_IN_EVENT)
                    .map(|m| SimilarMatch {
                        body_text: String::new(),
                        ..m.clone()
                    })
                    .collect();
                matches.sort_by(|a, b| {
                    b.similarity
                        .partial_cmp(&a.similarity)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                DuplicateWarning {
                    new_function: w.new_function.clone(),
                    matches,
                }
            })
            .collect()
    }

    /// Write the full, line-oriented overflow report and return the file path and per-warning
    /// offsets (1-indexed line number + record length) so the summary can embed `@line:len` refs.
    ///
    /// Format per record:
    /// ```text
    /// --- fn_name in rel/path.rs:10-25 (N matches)
    ///   best: other_fn in other/rel.rs:50-65 (96%)
    ///     | <up to MAX_CODE_PREVIEW_LINES lines of body_text>
    ///   also: another_fn in foo.rs:5-15 (91%)
    ///   also: ...
    ///                                         ← blank separator line
    /// ```
    fn write_overflow_report(
        warnings: &[DuplicateWarning],
        session_id: &str,
        workspace_root: Option<&std::path::Path>,
    ) -> Option<(std::path::PathBuf, Vec<WarningOffset>)> {
        let mut lines: Vec<String> = Vec::new();
        let mut offsets: Vec<WarningOffset> = Vec::with_capacity(warnings.len());

        for w in warnings {
            let nf = &w.new_function;
            let rel_file = Self::rel_path(&nf.file_path, workspace_root);
            let mut sorted_matches = w.matches.clone();
            sorted_matches.sort_by(|a, b| {
                b.similarity
                    .partial_cmp(&a.similarity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let record_start = lines.len() + 1; // 1-indexed

            // Record header
            lines.push(format!(
                "--- {} in {}:{}-{} ({} matches)",
                nf.function_name,
                rel_file,
                nf.start_line,
                nf.end_line,
                sorted_matches.len()
            ));

            // Best match: reference + code preview
            if let Some(top) = sorted_matches.first() {
                let mrel = Self::rel_path(&top.file_path, workspace_root);
                lines.push(format!(
                    "  best: {} in {}:{}-{} ({:.0}%)",
                    top.function_name,
                    mrel,
                    top.start_line,
                    top.end_line,
                    top.similarity * 100.0
                ));
                let body_lines: Vec<&str> = top.body_text.lines().collect();
                for bl in body_lines.iter().take(MAX_CODE_PREVIEW_LINES) {
                    lines.push(format!("    | {}", bl));
                }
                if body_lines.len() > MAX_CODE_PREVIEW_LINES {
                    lines.push(format!("    | ... ({} total lines)", body_lines.len()));
                }
            }

            // Remaining matches: references only
            for m in sorted_matches.iter().skip(1) {
                let mrel = Self::rel_path(&m.file_path, workspace_root);
                lines.push(format!(
                    "  also: {} in {}:{}-{} ({:.0}%)",
                    m.function_name,
                    mrel,
                    m.start_line,
                    m.end_line,
                    m.similarity * 100.0
                ));
            }

            // Blank separator
            lines.push(String::new());

            let record_len = lines.len() + 1 - record_start;
            offsets.push(WarningOffset {
                line: record_start,
                length: record_len,
            });
        }

        let content = lines.join("\n");

        // Write to /tmp/qmt-tool-outputs/{session_id}/dedup_review.txt
        let mut dir = std::env::temp_dir();
        dir.push("qmt-tool-outputs");
        dir.push(session_id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("DedupCheckMiddleware: failed to create overflow dir: {}", e);
            return None;
        }
        let file_path = dir.join("dedup_review.txt");
        if let Err(e) = std::fs::write(&file_path, &content) {
            log::warn!("DedupCheckMiddleware: failed to write overflow file: {}", e);
            return None;
        }

        Some((file_path, offsets))
    }

    /// Build the compact summary message injected into the LLM context.
    ///
    /// Groups warnings by their source file (descending warning count), shows at most
    /// `MAX_WARNINGS_IN_SUMMARY` entries inline.  Each entry carries an `@line:len`
    /// reference into the overflow file so the LLM can `read_file` for details.
    fn format_compact_summary(
        warnings: &[DuplicateWarning],
        offsets: &[WarningOffset],
        overflow_path: &std::path::Path,
        workspace_root: Option<&std::path::Path>,
    ) -> String {
        use std::collections::HashMap;

        // Group warnings by source file, preserving order
        let mut file_order: Vec<String> = Vec::new();
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new(); // rel_file -> [warning indices]
        for (i, w) in warnings.iter().enumerate() {
            let rel = Self::rel_path(&w.new_function.file_path, workspace_root);
            by_file.entry(rel.clone()).or_insert_with(|| {
                file_order.push(rel.clone());
                Vec::new()
            });
            by_file.get_mut(&rel).unwrap().push(i);
        }
        // Sort files by descending warning count
        file_order.sort_by(|a, b| by_file[b].len().cmp(&by_file[a].len()));

        let mut msg = format!(
            "\n📋 CODE REVIEW: {} potential duplicate(s) across {} file(s).\n\
             Full report: {}\n\
             Use read_file with the offset/limit refs below to see code previews.\n",
            warnings.len(),
            file_order.len(),
            overflow_path.display()
        );

        let mut shown = 0usize;
        'outer: for rel_file in &file_order {
            let indices = &by_file[rel_file];
            msg.push_str(&format!("\n{}  ({} functions)\n", rel_file, indices.len()));
            for &idx in indices {
                if shown >= MAX_WARNINGS_IN_SUMMARY {
                    let remaining = warnings.len() - shown;
                    msg.push_str(&format!("  ... +{} more — see full report\n", remaining));
                    break 'outer;
                }
                let w = &warnings[idx];
                let nf = &w.new_function;
                let off = &offsets[idx];

                // Best match summary (highest similarity)
                let best_str = w
                    .matches
                    .iter()
                    .max_by(|a, b| {
                        a.similarity
                            .partial_cmp(&b.similarity)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|m| {
                        let mname = m
                            .file_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| m.file_path.display().to_string());
                        let extra = if w.matches.len() > 1 {
                            format!(" +{}", w.matches.len() - 1)
                        } else {
                            String::new()
                        };
                        format!(
                            "~ {} in {} ({:.0}%){}",
                            m.function_name,
                            mname,
                            m.similarity * 100.0,
                            extra
                        )
                    })
                    .unwrap_or_default();

                msg.push_str(&format!(
                    "  {}:{}-{}  {}  @{}:{}\n",
                    nf.function_name, nf.start_line, nf.end_line, best_str, off.line, off.length
                ));
                shown += 1;
            }
        }

        msg.push_str(
            "\nACTION:\n\
             - If you moved/extracted these functions as part of a refactor, delete the originals.\n\
             - If unintentional, reuse the existing function instead.\n\
             - If the functions serve different purposes despite similar structure, no action needed.\n\
             TIP: read_file <report_path> offset=N limit=L to inspect any entry above.\n",
        );

        msg
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
        skip(self, workspace, changed_paths),
        fields(
            files_removed = %changed_paths.removed.len(),
            files_updated = tracing::field::Empty
        )
    )]
    async fn update_index(&self, workspace: &WorkspaceHandle, changed_paths: &DiffPaths) {
        // Remove deleted files
        for path in &changed_paths.removed {
            let _ = workspace
                .actor
                .tell(RemoveFile {
                    file_path: path.clone(),
                })
                .await;
        }

        // Update added/modified files
        let mut files_updated = 0usize;
        for path in changed_paths.changed_files() {
            if let Ok(source) = std::fs::read_to_string(path) {
                let _ = workspace
                    .actor
                    .tell(UpdateFile {
                        file_path: path.to_path_buf(),
                        source,
                    })
                    .await;
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
        log::debug!(
            "DedupCheckMiddleware: on_turn_end called, state={}",
            state.name()
        );

        if !self.enabled {
            log::debug!("DedupCheckMiddleware: skipping — disabled");
            tracing::Span::current().record("output_state", state.name());
            return Ok(state);
        }

        // Only run on Complete state
        if !matches!(state, ExecutionState::Complete) {
            log::debug!(
                "DedupCheckMiddleware: skipping — state is {} (not Complete)",
                state.name()
            );
            tracing::Span::current().record("output_state", state.name());
            return Ok(state);
        }

        // Get the last context for building BeforeLlmCall state
        let last_context = self.last_context.lock().await.clone();
        let Some(context) = last_context else {
            log::debug!("DedupCheckMiddleware: skipping — no last_context captured");
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        };

        // Get function_index and turn_diffs from the runtime parameter
        let Some(runtime) = runtime else {
            log::debug!("DedupCheckMiddleware: skipping — no runtime provided");
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        };

        // Guard: only review once per prompt generation.
        let current_token = ReviewToken {
            session_id: context.session_id.to_string(),
            generation: runtime
                .turn_generation
                .load(std::sync::atomic::Ordering::SeqCst),
        };
        {
            let mut guard = self.last_reviewed.lock().await;
            if let Some(last) = guard.as_ref()
                && last == &current_token
            {
                log::debug!(
                    "DedupCheckMiddleware: skipping — already reviewed this prompt generation"
                );
                tracing::Span::current().record("output_state", "Complete");
                return Ok(state);
            }
            *guard = Some(current_token);
        }

        let workspace = runtime.workspace_handle.get().cloned();
        let Some(workspace) = workspace else {
            log::debug!(
                "DedupCheckMiddleware: skipping — workspace index not ready yet \
                (index is still initializing in the background)"
            );
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
        };

        // Get and clear turn_diffs
        let turn_diffs = {
            let mut diffs = runtime.turn_diffs.lock();
            let accumulated = diffs.clone();
            *diffs = DiffPaths::default();
            accumulated
        };

        if turn_diffs.is_empty() {
            log::debug!("DedupCheckMiddleware: skipping — no file changes in this turn");
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
        let mut warnings = self.check_for_duplicates(&workspace, &turn_diffs).await;

        // Filter out moved functions
        warnings = self.filter_moved_functions(warnings, &turn_diffs);

        // Record duplicates found
        tracing::Span::current().record("duplicates_found", warnings.len());

        // Update the index with new/modified functions
        self.update_index(&workspace, &turn_diffs).await;

        // If duplicates found after filtering, write overflow report and inject compact summary
        if !warnings.is_empty() {
            log::info!(
                "DedupCheckMiddleware: Found {} duplicate code warnings in turn review",
                warnings.len()
            );

            // Workspace root for path relativisation
            let workspace_root: Option<std::path::PathBuf> =
                workspace.file_index().map(|fi| fi.root.clone());

            // 1. Write the full line-indexed overflow report to a temp file
            let overflow = Self::write_overflow_report(
                &warnings,
                &context.session_id,
                workspace_root.as_deref(),
            );
            let (overflow_path_opt, offsets) = match overflow {
                Some((path, offs)) => (Some(path), offs),
                None => (
                    None,
                    vec![WarningOffset { line: 0, length: 0 }; warnings.len()],
                ),
            };

            // 2. Compact warnings for the event payload (no body_text, capped matches)
            let compacted = Self::compact_warnings(&warnings);

            // 3. Emit event with compacted warnings + overflow path.
            // Intentionally ephemeral (transport-only) because payload
            // references temporary overflow files that won't survive restart.
            if let Some(ref event_sink) = self.event_sink {
                event_sink.emit_ephemeral(
                    context.session_id.as_ref(),
                    AgentEventKind::DuplicateCodeDetected {
                        warnings: compacted,
                        overflow_path: overflow_path_opt.as_ref().map(|p| p.display().to_string()),
                    },
                );
            }

            // 4. Build the compact summary message for the LLM (~5KB)
            let summary = if let Some(ref path) = overflow_path_opt {
                Self::format_compact_summary(&warnings, &offsets, path, workspace_root.as_deref())
            } else {
                // Fallback: overflow write failed — inject a minimal summary
                format!(
                    "\n📋 CODE REVIEW: {} potential duplicate(s) found.\n\
                     ACTION: Review recently written functions for duplication.\n",
                    warnings.len()
                )
            };

            // 5. Inject the compact summary into a new context
            let new_context = Arc::new(context.inject_message(summary));

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
        if let Ok(mut guard) = self.last_reviewed.try_lock() {
            *guard = None;
        }
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

        let mut mw = DedupCheckMiddleware::new().with_event_sink(agent_config.event_sink.clone());

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

    fn make_warning(
        fn_name: &str,
        file: &str,
        start: u32,
        end: u32,
        matches: Vec<SimilarMatch>,
    ) -> DuplicateWarning {
        DuplicateWarning {
            new_function: FunctionLocation {
                file_path: PathBuf::from(file),
                function_name: fn_name.to_string(),
                start_line: start,
                end_line: end,
            },
            matches,
        }
    }

    fn make_match(fn_name: &str, file: &str, similarity: f64, body: &str) -> SimilarMatch {
        SimilarMatch {
            file_path: PathBuf::from(file),
            function_name: fn_name.to_string(),
            start_line: 1,
            end_line: 10,
            similarity,
            body_text: body.to_string(),
        }
    }

    #[test]
    fn test_compact_warnings_strips_body_text() {
        let warnings = vec![make_warning(
            "calculateTotal",
            "src/utils.rs",
            10,
            20,
            vec![
                make_match("computeSum", "src/helpers.rs", 0.95, "fn computeSum() {}"),
                make_match("addUp", "src/math.rs", 0.90, "fn addUp() {}"),
                make_match("total", "src/total.rs", 0.85, "fn total() {}"),
                make_match("sum", "src/sum.rs", 0.82, "fn sum() {}"), // should be dropped
            ],
        )];

        let compacted = DedupCheckMiddleware::compact_warnings(&warnings);
        assert_eq!(compacted.len(), 1);
        // body_text stripped
        for m in &compacted[0].matches {
            assert!(m.body_text.is_empty(), "body_text should be empty");
        }
        // capped at MAX_MATCHES_IN_EVENT
        assert_eq!(compacted[0].matches.len(), MAX_MATCHES_IN_EVENT);
    }

    #[test]
    fn test_write_overflow_report_and_format_compact_summary() {
        let warnings = vec![
            make_warning(
                "calculateTotal",
                "src/utils.rs",
                10,
                20,
                vec![make_match(
                    "computeSum",
                    "src/helpers.rs",
                    0.85,
                    "fn computeSum() {\n    // body\n}",
                )],
            ),
            make_warning(
                "doThing",
                "src/utils.rs",
                30,
                40,
                vec![make_match(
                    "doOtherThing",
                    "src/other.rs",
                    0.90,
                    "fn doOtherThing() {}",
                )],
            ),
        ];

        let session_id = "test-session-dedup";
        let (path, offsets) =
            DedupCheckMiddleware::write_overflow_report(&warnings, session_id, None)
                .expect("write should succeed");

        // File should exist and be non-empty
        let content = std::fs::read_to_string(&path).expect("file should be readable");
        assert!(!content.is_empty());

        // First record should start at line 1
        assert_eq!(offsets[0].line, 1);
        // Second record should start after the first
        assert!(offsets[1].line > offsets[0].line);

        // Verify the content at each offset
        let lines: Vec<&str> = content.lines().collect();
        assert!(
            lines[offsets[0].line - 1].contains("calculateTotal"),
            "first record header should mention calculateTotal"
        );
        assert!(
            lines[offsets[1].line - 1].contains("doThing"),
            "second record header should mention doThing"
        );

        // Verify code preview appears in the file
        assert!(content.contains("| fn computeSum()"));

        // Now test the summary format
        let summary =
            DedupCheckMiddleware::format_compact_summary(&warnings, &offsets, &path, None);
        assert!(summary.contains("CODE REVIEW"));
        assert!(summary.contains("calculateTotal"));
        assert!(summary.contains("computeSum"));
        assert!(summary.contains("85%"));
        assert!(summary.contains("ACTION"));
        assert!(summary.contains("@1:")); // offset reference present
        // body_text should NOT be in the summary
        assert!(!summary.contains("fn computeSum()"));

        // Clean up
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_compact_summary_caps_at_max_warnings() {
        // Build MAX_WARNINGS_IN_SUMMARY + 5 warnings
        let warnings: Vec<DuplicateWarning> = (0..MAX_WARNINGS_IN_SUMMARY + 5)
            .map(|i| {
                make_warning(
                    &format!("fn_{}", i),
                    "src/big_file.rs",
                    (i * 10) as u32,
                    (i * 10 + 5) as u32,
                    vec![make_match("other", "src/other.rs", 0.90, "")],
                )
            })
            .collect();

        let session_id = "test-session-dedup-cap";
        let (path, offsets) =
            DedupCheckMiddleware::write_overflow_report(&warnings, session_id, None)
                .expect("write should succeed");

        let summary =
            DedupCheckMiddleware::format_compact_summary(&warnings, &offsets, &path, None);

        // Should mention the overflow
        assert!(summary.contains("more"));
        // Should not exceed a reasonable size (well under 50KB)
        assert!(
            summary.len() < 50_000,
            "summary should be compact, got {} bytes",
            summary.len()
        );

        let _ = std::fs::remove_file(&path);
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
            content: vec![querymt::chat::Content::text("ok")],
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

    // =========================================================================
    // Obvious Duplicate Detection Tests
    // =========================================================================
    //
    // These tests verify that the DedupCheckMiddleware correctly detects
    // obvious cases of code duplication that should never be missed.

    use crate::index::function_index::{FunctionIndex, FunctionIndexConfig};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// Test configuration for duplicate detection tests
    struct DedupTestConfig {
        threshold: f64,
        min_lines: u32,
        description: &'static str,
    }

    const TEST_CONFIGS: &[DedupTestConfig] = &[
        DedupTestConfig {
            threshold: 0.70,
            min_lines: 5,
            description: "low_threshold",
        },
        DedupTestConfig {
            threshold: 0.85,
            min_lines: 10,
            description: "default_single_coder",
        },
        DedupTestConfig {
            threshold: 0.90,
            min_lines: 5,
            description: "high_threshold",
        },
    ];

    /// Helper to create a FunctionIndex from a temp directory
    async fn build_index(temp_dir: &TempDir, config: &DedupTestConfig) -> FunctionIndex {
        let index_config = FunctionIndexConfig::default()
            .with_min_lines(config.min_lines)
            .with_threshold(config.threshold);
        FunctionIndex::build(temp_dir.path(), index_config)
            .await
            .expect("should build index")
    }

    /// Test: Exact copy-paste of a function MUST be detected
    #[tokio::test]
    async fn test_exact_copy_paste_detected() {
        // Function must be at least 10 lines to pass all config variants
        let original_fn = r#"
pub fn calculate_total(items: &[Item]) -> f64 {
    let mut total = 0.0;
    for item in items {
        let price = item.price;
        let quantity = item.quantity as f64;
        let subtotal = price * quantity;
        if subtotal > 0.0 {
            total += subtotal;
        }
    }
    total
}
"#;

        for config in TEST_CONFIGS {
            let temp_dir = TempDir::new().unwrap();

            // Create original file
            fs::write(temp_dir.path().join("utils.rs"), original_fn).unwrap();

            let index = build_index(&temp_dir, config).await;

            // Probe with exact same function in a "new" file
            let results = index.find_similar_to_code(Path::new("new_file.rs"), original_fn);

            assert!(
                !results.is_empty(),
                "[{}] Exact copy-paste should be detected (threshold={}, min_lines={})",
                config.description,
                config.threshold,
                config.min_lines
            );

            if !results.is_empty() {
                let similarity = results[0].1[0].similarity;
                assert!(
                    similarity >= 0.95,
                    "[{}] Exact copy should have similarity >= 0.95, got {:.4}",
                    config.description,
                    similarity
                );
            }
        }
    }

    /// Test: Function with renamed variables but identical structure MUST be detected
    #[tokio::test]
    async fn test_renamed_variables_detected() {
        let original_fn = r#"
pub fn process_data(input: &[Record]) -> Vec<Output> {
    let mut results = Vec::new();
    for record in input {
        if record.is_valid() {
            let transformed = record.transform();
            results.push(transformed);
        }
    }
    results
}
"#;

        let renamed_fn = r#"
pub fn handle_items(data: &[Record]) -> Vec<Output> {
    let mut outputs = Vec::new();
    for item in data {
        if item.is_valid() {
            let converted = item.transform();
            outputs.push(converted);
        }
    }
    outputs
}
"#;

        for config in TEST_CONFIGS {
            let temp_dir = TempDir::new().unwrap();

            fs::write(temp_dir.path().join("processor.rs"), original_fn).unwrap();

            let index = build_index(&temp_dir, config).await;

            let results = index.find_similar_to_code(Path::new("handler.rs"), renamed_fn);

            assert!(
                !results.is_empty(),
                "[{}] Renamed variables should still be detected as duplicate",
                config.description
            );

            if !results.is_empty() {
                let similarity = results[0].1[0].similarity;
                assert!(
                    similarity >= 0.80,
                    "[{}] Renamed variables should have high similarity, got {:.4}",
                    config.description,
                    similarity
                );
            }
        }
    }

    /// Test: Multiple copies of the same function in different files MUST all be detected
    /// This replicates the bug scenario where a function was copy-pasted 10 times.
    #[tokio::test]
    async fn test_multiple_copies_all_detected() {
        let original_fn = r#"
pub fn validate_input(value: &str, max_len: usize) -> Result<(), String> {
    if value.is_empty() {
        return Err("Input cannot be empty".to_string());
    }
    if value.len() > max_len {
        return Err(format!("Input exceeds max length of {}", max_len));
    }
    if !value.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err("Input contains invalid characters".to_string());
    }
    Ok(())
}
"#;

        let config = &TEST_CONFIGS[0]; // Use low threshold config
        let temp_dir = TempDir::new().unwrap();

        // Create original file
        fs::write(temp_dir.path().join("validation.rs"), original_fn).unwrap();

        let index = build_index(&temp_dir, config).await;

        // Create 10 "new" files with copies
        let mut detected_count = 0;
        for i in 0..10 {
            let new_file = format!("module_{}.rs", i);
            let results = index.find_similar_to_code(Path::new(&new_file), original_fn);
            if !results.is_empty() {
                detected_count += 1;
            }
        }

        assert_eq!(
            detected_count, 10,
            "All 10 copies should be detected as duplicates, got {}",
            detected_count
        );
    }

    /// Test: Specific regression test for the u32_from_usize scenario
    /// This was the actual function that was copy-pasted multiple times without detection.
    #[tokio::test]
    async fn test_u32_from_usize_scenario() {
        let original_fn = r#"
pub fn u32_from_usize(value: usize, field_name: &str, session_id: Option<&str>) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| {
        log::warn!(
            "{}={} exceeds u32 max (session: {:?})",
            field_name,
            value,
            session_id
        );
        u32::MAX
    })
}
"#;

        // Variants with minor changes that should still be detected
        let variants = [
            // Exact copy
            r#"
pub fn u32_from_usize(value: usize, field_name: &str, session_id: Option<&str>) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| {
        log::warn!(
            "{}={} exceeds u32 max (session: {:?})",
            field_name,
            value,
            session_id
        );
        u32::MAX
    })
}
"#,
            // Renamed function
            r#"
pub fn convert_to_u32(val: usize, name: &str, sess_id: Option<&str>) -> u32 {
    u32::try_from(val).unwrap_or_else(|_| {
        log::warn!(
            "{}={} exceeds u32 max (session: {:?})",
            name,
            val,
            sess_id
        );
        u32::MAX
    })
}
"#,
            // Slightly different log message
            r#"
pub fn safe_u32_convert(value: usize, field: &str, session: Option<&str>) -> u32 {
    u32::try_from(value).unwrap_or_else(|_| {
        log::warn!(
            "Field {}={} too large for u32 (session: {:?})",
            field,
            value,
            session
        );
        u32::MAX
    })
}
"#,
        ];

        let config = DedupTestConfig {
            threshold: 0.75,
            min_lines: 5u32,
            description: "u32_from_usize_test",
        };
        let temp_dir = TempDir::new().unwrap();

        fs::write(temp_dir.path().join("utils.rs"), original_fn).unwrap();

        let index = build_index(&temp_dir, &config).await;

        assert!(
            index.function_count() >= 1,
            "Should index at least one function"
        );

        for (i, variant) in variants.iter().enumerate() {
            let new_file = format!("copy_{}.rs", i);
            let results = index.find_similar_to_code(Path::new(&new_file), variant);

            assert!(
                !results.is_empty(),
                "Variant {} should be detected as duplicate of u32_from_usize",
                i
            );

            if !results.is_empty() {
                let similarity = results[0].1[0].similarity;
                assert!(
                    similarity >= 0.70,
                    "Variant {} should have similarity >= 0.70, got {:.4}",
                    i,
                    similarity
                );
            }
        }
    }

    /// Test: filter_moved_functions correctly filters out functions that were moved
    #[tokio::test]
    async fn test_filter_moved_functions_works() {
        let mw = DedupCheckMiddleware::new();

        // Create a warning where the match is in a file that was removed (moved scenario)
        let warnings = vec![DuplicateWarning {
            new_function: FunctionLocation {
                file_path: PathBuf::from("src/new_location.rs"),
                function_name: "my_function".to_string(),
                start_line: 1,
                end_line: 10,
            },
            matches: vec![SimilarMatch {
                file_path: PathBuf::from("src/old_location.rs"),
                function_name: "my_function".to_string(),
                start_line: 1,
                end_line: 10,
                similarity: 0.99,
                body_text: "fn my_function() {}".to_string(),
            }],
        }];

        // Simulate move: old_location.rs was removed, new_location.rs was added
        let changed_paths = DiffPaths {
            added: vec![PathBuf::from("src/new_location.rs")],
            modified: vec![],
            removed: vec![PathBuf::from("src/old_location.rs")],
        };

        let filtered = mw.filter_moved_functions(warnings, &changed_paths);

        assert!(
            filtered.is_empty(),
            "Moved functions should be filtered out, got {} warnings",
            filtered.len()
        );
    }

    /// Test: filter_moved_functions keeps warnings for actual duplicates (not moves)
    #[tokio::test]
    async fn test_filter_moved_functions_keeps_real_duplicates() {
        let mw = DedupCheckMiddleware::new();

        // Create a warning where the match is in a file that was NOT removed
        let warnings = vec![DuplicateWarning {
            new_function: FunctionLocation {
                file_path: PathBuf::from("src/new_file.rs"),
                function_name: "my_function".to_string(),
                start_line: 1,
                end_line: 10,
            },
            matches: vec![SimilarMatch {
                file_path: PathBuf::from("src/existing_file.rs"),
                function_name: "similar_function".to_string(),
                start_line: 1,
                end_line: 10,
                similarity: 0.95,
                body_text: "fn similar_function() {}".to_string(),
            }],
        }];

        // Only new_file.rs was added, existing_file.rs was not touched
        let changed_paths = DiffPaths {
            added: vec![PathBuf::from("src/new_file.rs")],
            modified: vec![],
            removed: vec![],
        };

        let filtered = mw.filter_moved_functions(warnings, &changed_paths);

        assert_eq!(
            filtered.len(),
            1,
            "Real duplicates should not be filtered out"
        );
    }

    /// Test: TypeScript/JavaScript duplicate detection works
    #[tokio::test]
    async fn test_typescript_duplicate_detection() {
        let original_ts = r#"
function calculateDiscount(price: number, percentage: number): number {
    const discount = price * (percentage / 100);
    const finalPrice = price - discount;
    if (finalPrice < 0) {
        return 0;
    }
    return finalPrice;
}
"#;

        let copy_ts = r#"
function computeDiscount(amount: number, percent: number): number {
    const reduction = amount * (percent / 100);
    const result = amount - reduction;
    if (result < 0) {
        return 0;
    }
    return result;
}
"#;

        let config = &TEST_CONFIGS[0];
        let temp_dir = TempDir::new().unwrap();

        fs::write(temp_dir.path().join("pricing.ts"), original_ts).unwrap();

        let index = build_index(&temp_dir, config).await;

        let results = index.find_similar_to_code(Path::new("discounts.ts"), copy_ts);

        assert!(
            !results.is_empty(),
            "TypeScript duplicate should be detected"
        );
    }

    /// Test: Python duplicate detection works
    #[tokio::test]
    async fn test_python_duplicate_detection() {
        let original_py = r#"
def process_items(items):
    results = []
    for item in items:
        if item.is_valid():
            processed = item.transform()
            results.append(processed)
    return results
"#;

        let copy_py = r#"
def handle_records(records):
    output = []
    for record in records:
        if record.is_valid():
            converted = record.transform()
            output.append(converted)
    return output
"#;

        let config = &TEST_CONFIGS[0];
        let temp_dir = TempDir::new().unwrap();

        fs::write(temp_dir.path().join("processor.py"), original_py).unwrap();

        let index = build_index(&temp_dir, config).await;

        let results = index.find_similar_to_code(Path::new("handler.py"), copy_py);

        assert!(!results.is_empty(), "Python duplicate should be detected");
    }

    /// Test: Different functions should NOT be flagged as duplicates
    #[tokio::test]
    async fn test_different_functions_not_flagged() {
        let fn_a = r#"
pub fn serialize_to_json(data: &Data) -> String {
    let mut result = String::from("{");
    result.push_str(&format!("\"name\": \"{}\",", data.name));
    result.push_str(&format!("\"value\": {}", data.value));
    result.push('}');
    result
}
"#;

        let fn_b = r#"
pub fn connect_database(url: &str) -> Result<Connection, Error> {
    let config = parse_connection_string(url)?;
    let pool = create_pool(&config)?;
    let conn = pool.get_connection()?;
    conn.ping()?;
    Ok(conn)
}
"#;

        let config = &TEST_CONFIGS[1]; // Default threshold
        let temp_dir = TempDir::new().unwrap();

        fs::write(temp_dir.path().join("serializer.rs"), fn_a).unwrap();

        let index = build_index(&temp_dir, config).await;

        let results = index.find_similar_to_code(Path::new("database.rs"), fn_b);

        assert!(
            results.is_empty(),
            "Completely different functions should not be flagged as duplicates"
        );
    }

    // =========================================================================
    // Middleware Integration Tests (Seeded turn_diffs)
    // =========================================================================
    //
    // NOTE: These tests seed `runtime.turn_diffs` directly and validate middleware
    // behavior once diffs already exist. They do NOT validate the execution engine
    // path that populates turn_diffs from tool-result snapshots.
    //
    // A true end-to-end producer+consumer contract test lives in
    // `agent/execution_tests.rs` (ignored test: `test_e2e_tool_call_populates_turn_diffs_for_turn_end_middleware`).
    //
    // Marked #[ignore] because they are slow and require real workspace indexing.

    use crate::agent::core::{AgentMode, McpToolState, SessionRuntime};
    use crate::index::file_index::FileIndexConfig;
    use crate::index::workspace_actor::WorkspaceIndexActor;
    use crate::middleware::{AgentStats, ConversationContext, ExecutionState, LlmResponse};
    use std::sync::Arc;

    /// Full integration test: verify the entire dedup detection stack works
    #[tokio::test]
    #[ignore] // Slow test - run with `cargo test -- --ignored`
    async fn test_integration_full_stack_duplicate_detection() {
        for config in TEST_CONFIGS {
            run_full_stack_test(config).await;
        }
    }

    async fn run_full_stack_test(config: &DedupTestConfig) {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();

        // Create an original function in the workspace (12 lines to pass min_lines=10)
        let original_fn = r#"
pub fn calculate_sum(numbers: &[i32]) -> i32 {
    let mut sum = 0;
    for num in numbers {
        let value = *num;
        if value > 0 {
            sum += value;
        } else {
            sum -= value.abs();
        }
    }
    sum
}
"#;
        fs::write(root.join("math_utils.rs"), original_fn).unwrap();

        // Build the workspace index
        let index_config = FunctionIndexConfig::default()
            .with_min_lines(config.min_lines)
            .with_threshold(config.threshold);

        let workspace_handle =
            WorkspaceIndexActor::create(root.clone(), FileIndexConfig::default(), index_config)
                .await
                .expect("should create workspace handle");

        // Create SessionRuntime and set workspace_handle
        let runtime = SessionRuntime::new(
            Some(root.clone()),
            std::collections::HashMap::new(),
            McpToolState::empty(),
        );

        // Set the workspace handle (ignore error if already set)
        let _ = runtime.workspace_handle.set(workspace_handle);

        // Create a new file with a copy of the function (same structure, different names)
        let copy_fn = r#"
pub fn compute_total(values: &[i32]) -> i32 {
    let mut total = 0;
    for val in values {
        let amount = *val;
        if amount > 0 {
            total += amount;
        } else {
            total -= amount.abs();
        }
    }
    total
}
"#;
        let new_file_path = root.join("new_module.rs");
        fs::write(&new_file_path, copy_fn).unwrap();

        // Populate turn_diffs as if a tool wrote this file
        {
            let mut diffs = runtime.turn_diffs.lock();
            diffs.added.push(new_file_path);
        }

        // Create middleware
        let mw = DedupCheckMiddleware::new()
            .threshold(config.threshold)
            .min_lines(config.min_lines as usize);

        // Create a conversation context
        let context = Arc::new(ConversationContext {
            session_id: "test-integration-session".into(),
            messages: Arc::from([]),
            stats: Arc::new(AgentStats::default()),
            provider: "test".into(),
            model: "test-model".into(),
            session_mode: AgentMode::Build,
        });

        // First, capture the context via on_after_llm
        let llm_response = LlmResponse::new(String::new(), vec![], None, None);
        let state = ExecutionState::AfterLlm {
            context: context.clone(),
            response: Arc::new(llm_response),
        };

        let _ = mw.on_after_llm(state, Some(&runtime)).await;

        // Now call on_turn_end with Complete state
        let complete_state = ExecutionState::Complete;
        let result = mw.on_turn_end(complete_state, Some(&runtime)).await;

        match result {
            Ok(ExecutionState::BeforeLlmCall { context: new_ctx }) => {
                // Verify the injected message contains duplicate warning
                let last_message = new_ctx.messages.last();
                assert!(
                    last_message.is_some(),
                    "[{}] Should have injected a message",
                    config.description
                );

                if let Some(msg) = last_message {
                    let content_str: String = msg
                        .content
                        .iter()
                        .filter_map(|c| c.as_text())
                        .collect::<Vec<_>>()
                        .join(" ");
                    let has_dedup_content =
                        content_str.contains("CODE REVIEW") || content_str.contains("duplicate");
                    assert!(
                        has_dedup_content,
                        "[{}] Injected message should contain duplicate warning, got: {:?}",
                        config.description,
                        &content_str[..content_str.len().min(200)]
                    );
                }
            }
            Ok(ExecutionState::Complete) => {
                // This might happen if similarity is below threshold
                // Check if we expected detection
                panic!(
                    "[{}] Expected BeforeLlmCall with duplicate warning, got Complete. \
                    This may indicate the duplicate detection failed.",
                    config.description
                );
            }
            Ok(other) => {
                panic!(
                    "[{}] Unexpected state: {:?}",
                    config.description,
                    other.name()
                );
            }
            Err(e) => {
                panic!("[{}] on_turn_end failed: {:?}", config.description, e);
            }
        }
    }

    /// Integration test: verify turn_diffs is cleared after processing
    #[tokio::test]
    #[ignore]
    async fn test_integration_turn_diffs_cleared_after_processing() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();

        // Create a simple file
        fs::write(root.join("test.rs"), "fn main() {}").unwrap();

        let workspace_handle = WorkspaceIndexActor::create(
            root.clone(),
            FileIndexConfig::default(),
            FunctionIndexConfig::default(),
        )
        .await
        .expect("should create workspace handle");

        let runtime = SessionRuntime::new(
            Some(root.clone()),
            std::collections::HashMap::new(),
            McpToolState::empty(),
        );

        let _ = runtime.workspace_handle.set(workspace_handle);

        // Populate turn_diffs
        {
            let mut diffs = runtime.turn_diffs.lock();
            diffs.added.push(root.join("new_file.rs"));
        }

        // Verify diffs are populated
        {
            let diffs = runtime.turn_diffs.lock();
            assert!(!diffs.is_empty(), "turn_diffs should be populated");
        }

        let mw = DedupCheckMiddleware::new();

        // Capture context
        let context = Arc::new(ConversationContext {
            session_id: "test-clear-session".into(),
            messages: Arc::from([]),
            stats: Arc::new(AgentStats::default()),
            provider: "test".into(),
            model: "test-model".into(),
            session_mode: AgentMode::Build,
        });

        let llm_response = LlmResponse::new(String::new(), vec![], None, None);
        let state = ExecutionState::AfterLlm {
            context: context.clone(),
            response: Arc::new(llm_response),
        };
        let _ = mw.on_after_llm(state, Some(&runtime)).await;

        // Call on_turn_end
        let _ = mw
            .on_turn_end(ExecutionState::Complete, Some(&runtime))
            .await;

        // Verify turn_diffs is cleared
        {
            let diffs = runtime.turn_diffs.lock();
            assert!(
                diffs.is_empty(),
                "turn_diffs should be cleared after on_turn_end"
            );
        }
    }

    /// Integration test: verify middleware skips when workspace_handle not set
    #[tokio::test]
    #[ignore]
    async fn test_integration_skips_without_workspace_handle() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();

        // Create runtime WITHOUT setting workspace_handle
        let runtime = SessionRuntime::new(
            Some(root),
            std::collections::HashMap::new(),
            McpToolState::empty(),
        );

        let mw = DedupCheckMiddleware::new();

        let context = Arc::new(ConversationContext {
            session_id: "test-no-workspace".into(),
            messages: Arc::from([]),
            stats: Arc::new(AgentStats::default()),
            provider: "test".into(),
            model: "test-model".into(),
            session_mode: AgentMode::Build,
        });

        // Capture context
        let llm_response = LlmResponse::new(String::new(), vec![], None, None);
        let state = ExecutionState::AfterLlm {
            context: context.clone(),
            response: Arc::new(llm_response),
        };
        let _ = mw.on_after_llm(state, Some(&runtime)).await;

        // Should return Complete unchanged (skipped)
        let result = mw
            .on_turn_end(ExecutionState::Complete, Some(&runtime))
            .await;

        assert!(
            matches!(result, Ok(ExecutionState::Complete)),
            "Should return Complete when workspace_handle not set"
        );
    }

    /// Integration test: verify middleware skips when disabled
    #[tokio::test]
    #[ignore]
    async fn test_integration_skips_when_disabled() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();

        fs::write(root.join("test.rs"), "fn foo() { 1 + 1 }").unwrap();

        let workspace_handle = WorkspaceIndexActor::create(
            root.clone(),
            FileIndexConfig::default(),
            FunctionIndexConfig::default(),
        )
        .await
        .unwrap();

        let runtime = SessionRuntime::new(
            Some(root.clone()),
            std::collections::HashMap::new(),
            McpToolState::empty(),
        );
        let _ = runtime.workspace_handle.set(workspace_handle);

        // Populate turn_diffs
        {
            let mut diffs = runtime.turn_diffs.lock();
            diffs.added.push(root.join("new.rs"));
        }

        // Create DISABLED middleware
        let mw = DedupCheckMiddleware::new().enabled(false);

        let context = Arc::new(ConversationContext {
            session_id: "test-disabled".into(),
            messages: Arc::from([]),
            stats: Arc::new(AgentStats::default()),
            provider: "test".into(),
            model: "test-model".into(),
            session_mode: AgentMode::Build,
        });

        let llm_response = LlmResponse::new(String::new(), vec![], None, None);
        let state = ExecutionState::AfterLlm {
            context,
            response: Arc::new(llm_response),
        };
        let _ = mw.on_after_llm(state, Some(&runtime)).await;

        let result = mw
            .on_turn_end(ExecutionState::Complete, Some(&runtime))
            .await;

        assert!(
            matches!(result, Ok(ExecutionState::Complete)),
            "Should return Complete when middleware is disabled"
        );
    }
}
