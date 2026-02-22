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
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tracing::instrument;

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

        // Guard: only review once per turn
        if self.already_reviewed_this_turn.swap(true, Ordering::SeqCst) {
            log::debug!("DedupCheckMiddleware: skipping — already reviewed this turn");
            tracing::Span::current().record("output_state", "Complete");
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
        let turn_diffs = runtime.turn_diffs.lock().ok().map(|mut diffs| {
            let accumulated = diffs.clone();
            *diffs = DiffPaths::default();
            accumulated
        });

        let Some(turn_diffs) = turn_diffs else {
            log::debug!("DedupCheckMiddleware: skipping — turn_diffs mutex poisoned");
            tracing::Span::current().record("files_checked", 0usize);
            tracing::Span::current().record("duplicates_found", 0usize);
            tracing::Span::current().record("review_injected", false);
            tracing::Span::current().record("output_state", "Complete");
            return Ok(state);
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

            // 3. Emit event with compacted warnings + overflow path
            if let Some(ref event_bus) = self.event_bus {
                event_bus.publish(
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
