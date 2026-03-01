//! Deterministic tool result compaction middleware (PR-5).
//!
//! Opt-in middleware that compacts large tool results from non-routed
//! (legacy) tool calls. Hooks into `ProcessingToolCalls` and transforms
//! results that meet all gating criteria:
//!
//! 1. Global feature is enabled (`tool_result_compaction.enabled`).
//! 2. The tool is in the `opt_in_tools` list.
//! 3. The result payload size >= `min_bytes`.
//! 4. The result is non-error (errors pass through unchanged).
//!
//! Deterministic transforms applied (no LLM calls):
//! - **Text/logs**: de-duplicate consecutive identical lines, preserve
//!   warnings/errors, head + tail window.
//! - **JSON-ish**: extract schema/keys/counts with bounded samples.
//! - **Fallback**: conservative truncation with retrieval reference hint.
//!
//! Full output references are preserved through overflow metadata so the
//! agent can retrieve the complete output via retrieval tools.
//!
//! # Configuration
//!
//! ```toml
//! [agent.execution.tool_result_compaction]
//! enabled = true
//! opt_in_tools = ["shell", "web_fetch"]
//! min_bytes = 8192
//! ```
//!
//! # Rollout & Rollback
//!
//! - **Enable**: set `enabled = true` and populate `opt_in_tools`.
//! - **Rollback**: set `enabled = false` — all behavior reverts immediately,
//!   no data migration required.
//! - **Gradual rollout**: start with a single tool (e.g. `["shell"]`), monitor
//!   the `compaction.process` tracing span for `bytes_saved` and `compacted`
//!   fields, then expand to more tools.
//!
//! # Observability
//!
//! All telemetry uses the `tracing` crate with structured fields:
//!
//! - `compaction.process` span: `results_count`, `compacted`, `skipped`, `bytes_saved`
//! - Per-result `debug!`: `tool_name`, `call_id`, `original_bytes`, `compacted_bytes`,
//!   `content_kind`, `compression_pct`
//! - Per-skip `trace!`: `tool_name`, `call_id`, `skip_reason`
//! - Batch summary `info!`: `compacted`, `skipped`, `bytes_saved`, cumulative counters

use crate::agent::core::SessionRuntime;
use crate::config::ToolResultCompactionConfig;
use crate::middleware::{ExecutionState, MiddlewareDriver, Result, ToolResult};
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::{debug, info, info_span, trace};

// ============================================================================
// Constants
// ============================================================================

/// Number of lines to keep from the head of text output.
const HEAD_LINES: usize = 80;

/// Number of lines to keep from the tail of text output.
const TAIL_LINES: usize = 80;

/// Maximum number of JSON keys to show in a schema summary.
const MAX_JSON_KEYS: usize = 30;

/// Maximum number of JSON array sample elements.
const MAX_JSON_SAMPLES: usize = 3;

/// Maximum byte length for a single JSON sample value shown in the preview.
const MAX_SAMPLE_VALUE_BYTES: usize = 200;

/// Patterns that indicate a line is a warning or error (case-insensitive prefix match).
const IMPORTANT_LINE_PATTERNS: &[&str] = &[
    "error",
    "err:",
    "warn:",
    "warning",
    "fatal",
    "panic",
    "fail",
    "failed",
    "exception",
    "traceback",
    "abort",
];

// ============================================================================
// Content detection
// ============================================================================

/// Heuristic content type of a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    /// Looks like JSON (object or array at top level).
    Json,
    /// Text / log output (default).
    Text,
}

fn detect_content_kind(content: &str) -> ContentKind {
    let trimmed = content.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        // Quick validation: try parsing as JSON
        if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
            return ContentKind::Json;
        }
    }
    ContentKind::Text
}

// ============================================================================
// Deterministic transforms
// ============================================================================

/// Compact text/log content: de-dup, preserve important lines, head+tail.
fn compact_text(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if total_lines <= HEAD_LINES + TAIL_LINES {
        // Content is small enough — no compaction needed even if bytes were over threshold.
        // This case is reached when lines are very long.
        return compact_long_lines(content, total_lines);
    }

    // Collect important lines (warnings, errors) from the middle
    let middle_start = HEAD_LINES;
    let middle_end = total_lines.saturating_sub(TAIL_LINES);
    let middle = &lines[middle_start..middle_end];

    let mut important_lines: Vec<(usize, &str)> = Vec::new();
    for (offset, line) in middle.iter().enumerate() {
        let lower = line.to_lowercase();
        if IMPORTANT_LINE_PATTERNS
            .iter()
            .any(|pat| lower.starts_with(pat) || lower.contains(&format!(" {pat}")))
        {
            important_lines.push((middle_start + offset + 1, line)); // 1-indexed
        }
    }

    // De-dup: count consecutive identical lines in the head
    let head = dedup_consecutive(&lines[..HEAD_LINES]);
    let tail = dedup_consecutive(&lines[middle_end..]);

    let omitted = middle_end - middle_start;
    let important_count = important_lines.len();

    let mut result = String::with_capacity(content.len() / 2);
    result.push_str(&format!(
        "[Compacted: {total_lines} lines total, showing head({HEAD_LINES}) + tail({TAIL_LINES}), \
         {omitted} lines omitted"
    ));
    if important_count > 0 {
        result.push_str(&format!(", {important_count} important lines preserved"));
    }
    result.push_str("]\n\n");

    // Head section
    result.push_str("--- HEAD ---\n");
    result.push_str(&head);
    result.push('\n');

    // Important lines from middle
    if !important_lines.is_empty() {
        result.push_str("\n--- IMPORTANT (from omitted section) ---\n");
        for (line_num, line) in important_lines.iter().take(50) {
            result.push_str(&format!("L{line_num}: {line}\n"));
        }
    }

    // Tail section
    result.push_str("\n--- TAIL ---\n");
    result.push_str(&tail);

    result.push_str("\n\n[Full output available via retrieval. Use context_search to query the indexed content.]");

    result
}

/// Handle case where the output has few lines but they are very long.
fn compact_long_lines(content: &str, total_lines: usize) -> String {
    // Truncate at a reasonable character limit
    let max_chars = (HEAD_LINES + TAIL_LINES) * 120;
    if content.len() <= max_chars {
        return content.to_string();
    }

    let half = max_chars / 2;
    let head = &content[..half];
    let tail = &content[content.len() - half..];

    format!(
        "[Compacted: {total_lines} lines, {} bytes total, showing head+tail]\n\n\
         --- HEAD ---\n{head}\n\n\
         --- [{} bytes omitted] ---\n\n\
         --- TAIL ---\n{tail}\n\n\
         [Full output available via retrieval. Use context_search to query the indexed content.]",
        content.len(),
        content.len() - max_chars,
    )
}

/// De-duplicate consecutive identical lines, replacing runs with a count.
fn dedup_consecutive(lines: &[&str]) -> String {
    let mut result = String::new();
    let mut prev: Option<&str> = None;
    let mut run_count: usize = 0;

    for line in lines {
        if Some(*line) == prev {
            run_count += 1;
        } else {
            if run_count > 1 {
                result.push_str(&format!("  ... (repeated {} more times)\n", run_count - 1));
            }
            if let Some(_) = prev {
                // Previous line was already pushed
            }
            result.push_str(line);
            result.push('\n');
            prev = Some(line);
            run_count = 1;
        }
    }

    if run_count > 1 {
        result.push_str(&format!("  ... (repeated {} more times)\n", run_count - 1));
    }

    result
}

/// Compact JSON content: extract schema/keys/counts with bounded samples.
fn compact_json(content: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(content.trim_start()) {
        Ok(v) => v,
        Err(_) => return compact_text(content), // fallback
    };

    let mut result = String::with_capacity(content.len() / 4);
    result.push_str(&format!(
        "[Compacted JSON: {} bytes original]\n\n",
        content.len()
    ));

    describe_json_value(&value, &mut result, 0, true);

    result.push_str(
        "\n\n[Full JSON available via retrieval. Use context_search to query the indexed content.]",
    );

    result
}

/// Recursively describe a JSON value with bounded depth and sampling.
fn describe_json_value(value: &serde_json::Value, out: &mut String, depth: usize, top: bool) {
    let indent = "  ".repeat(depth);

    match value {
        serde_json::Value::Object(map) => {
            let key_count = map.len();
            if top {
                out.push_str(&format!("{indent}Object with {key_count} keys:\n"));
            }

            let keys: Vec<&String> = map.keys().collect();
            for (i, key) in keys.iter().enumerate() {
                if i >= MAX_JSON_KEYS {
                    out.push_str(&format!(
                        "{indent}  ... and {} more keys\n",
                        key_count - MAX_JSON_KEYS
                    ));
                    break;
                }
                let val = &map[key.as_str()];
                let type_desc = json_type_summary(val);
                out.push_str(&format!("{indent}  \"{key}\": {type_desc}"));

                // Show a sample for simple values
                if let Some(sample) = json_sample(val) {
                    out.push_str(&format!(" = {sample}"));
                }
                out.push('\n');

                // Recurse one level for nested objects/arrays (only at top level)
                if depth == 0 {
                    match val {
                        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
                            describe_json_value(val, out, depth + 1, false);
                        }
                        _ => {}
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            let arr_len = arr.len();
            out.push_str(&format!("{indent}Array with {arr_len} elements"));

            if arr_len > 0 {
                let element_type = json_type_summary(&arr[0]);
                out.push_str(&format!(" (element type: {element_type})"));
            }
            out.push('\n');

            // Show samples
            for (i, elem) in arr.iter().enumerate() {
                if i >= MAX_JSON_SAMPLES {
                    out.push_str(&format!(
                        "{indent}  ... and {} more elements\n",
                        arr_len - MAX_JSON_SAMPLES
                    ));
                    break;
                }
                out.push_str(&format!("{indent}  [{i}]: "));
                let sample_str = serde_json::to_string(elem).unwrap_or_default();
                if sample_str.len() > MAX_SAMPLE_VALUE_BYTES {
                    out.push_str(&sample_str[..MAX_SAMPLE_VALUE_BYTES]);
                    out.push_str("...");
                } else {
                    out.push_str(&sample_str);
                }
                out.push('\n');
            }
        }
        other => {
            if let Some(sample) = json_sample(other) {
                out.push_str(&format!("{indent}{sample}\n"));
            }
        }
    }
}

/// Return a short type description for a JSON value.
fn json_type_summary(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(_) => "boolean".to_string(),
        serde_json::Value::Number(_) => "number".to_string(),
        serde_json::Value::String(s) => format!("string(len={})", s.len()),
        serde_json::Value::Array(arr) => format!("array(len={})", arr.len()),
        serde_json::Value::Object(map) => format!("object(keys={})", map.len()),
    }
}

/// Return a bounded sample string for simple JSON values.
fn json_sample(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            if s.len() <= MAX_SAMPLE_VALUE_BYTES {
                Some(format!("\"{}\"", s))
            } else {
                Some(format!("\"{}...\"", &s[..MAX_SAMPLE_VALUE_BYTES]))
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        serde_json::Value::Null => Some("null".to_string()),
        _ => None,
    }
}

/// Fallback compaction: conservative truncation with retrieval hint.
fn compact_fallback(content: &str) -> String {
    let max_bytes = (HEAD_LINES + TAIL_LINES) * 120;
    if content.len() <= max_bytes {
        return content.to_string();
    }

    let half = max_bytes / 2;
    let head = &content[..half];
    let tail = &content[content.len() - half..];

    format!(
        "[Compacted: {} bytes total, showing head+tail]\n\n\
         {head}\n\n\
         --- [{} bytes omitted] ---\n\n\
         {tail}\n\n\
         [Full output available via retrieval. Use context_search to query the indexed content.]",
        content.len(),
        content.len() - max_bytes,
    )
}

// ============================================================================
// Compaction metrics (for PR-8 telemetry)
// ============================================================================

/// Telemetry counters for compaction operations within a single middleware instance.
#[derive(Debug, Default)]
pub struct CompactionMetrics {
    /// Number of tool results that were compacted.
    pub compacted_count: AtomicU64,
    /// Number of tool results that were skipped (not opt-in, under threshold, or error).
    pub skipped_count: AtomicU64,
    /// Total bytes before compaction (across all compacted results).
    pub bytes_before: AtomicU64,
    /// Total bytes after compaction (across all compacted results).
    pub bytes_after: AtomicU64,
}

impl CompactionMetrics {
    fn record_compaction(&self, before: usize, after: usize) {
        self.compacted_count.fetch_add(1, Ordering::Relaxed);
        self.bytes_before
            .fetch_add(before as u64, Ordering::Relaxed);
        self.bytes_after.fetch_add(after as u64, Ordering::Relaxed);
    }

    fn record_skip(&self) {
        self.skipped_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot current counters for event emission.
    pub fn snapshot(&self) -> CompactionMetricsSnapshot {
        CompactionMetricsSnapshot {
            compacted_count: self.compacted_count.load(Ordering::Relaxed),
            skipped_count: self.skipped_count.load(Ordering::Relaxed),
            bytes_before: self.bytes_before.load(Ordering::Relaxed),
            bytes_after: self.bytes_after.load(Ordering::Relaxed),
        }
    }
}

/// Immutable snapshot of compaction metrics for event reporting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompactionMetricsSnapshot {
    pub compacted_count: u64,
    pub skipped_count: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl CompactionMetricsSnapshot {
    /// Compression ratio: bytes_after / bytes_before. Returns 1.0 if no bytes processed.
    pub fn compression_ratio(&self) -> f64 {
        if self.bytes_before == 0 {
            1.0
        } else {
            self.bytes_after as f64 / self.bytes_before as f64
        }
    }
}

/// Why a tool result was skipped during compaction (for debug traces).
#[derive(Debug, Clone, Copy)]
pub enum SkipReason {
    /// Feature is globally disabled.
    FeatureDisabled,
    /// Tool is not in the opt_in_tools list.
    NotOptIn,
    /// Payload size is below min_bytes threshold.
    BelowThreshold,
    /// Result is an error (errors pass through unchanged).
    IsError,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::FeatureDisabled => write!(f, "feature_disabled"),
            SkipReason::NotOptIn => write!(f, "not_opt_in"),
            SkipReason::BelowThreshold => write!(f, "below_threshold"),
            SkipReason::IsError => write!(f, "is_error"),
        }
    }
}

// ============================================================================
// Middleware implementation
// ============================================================================

/// Deterministic tool result compaction middleware.
///
/// Hooks into `ProcessingToolCalls` to transform large tool results
/// into bounded previews. Full output is preserved through retrieval
/// references.
pub struct ToolResultCompactionMiddleware {
    config: ToolResultCompactionConfig,
    active: AtomicBool,
    /// Telemetry counters.
    pub metrics: CompactionMetrics,
}

impl ToolResultCompactionMiddleware {
    pub fn new(config: ToolResultCompactionConfig) -> Self {
        let active = config.enabled && !config.opt_in_tools.is_empty();
        Self {
            config,
            active: AtomicBool::new(active),
            metrics: CompactionMetrics::default(),
        }
    }

    /// Check if a tool result should be compacted.
    fn should_compact(&self, result: &ToolResult) -> std::result::Result<(), SkipReason> {
        if !self.config.enabled {
            return Err(SkipReason::FeatureDisabled);
        }

        if result.is_error {
            return Err(SkipReason::IsError);
        }

        let tool_name = result.tool_name.as_deref().unwrap_or("");

        if !self.config.opt_in_tools.iter().any(|t| t == tool_name) {
            return Err(SkipReason::NotOptIn);
        }

        if result.content.len() < self.config.min_bytes {
            return Err(SkipReason::BelowThreshold);
        }

        Ok(())
    }

    /// Apply deterministic compaction to a tool result.
    fn compact_result(&self, content: &str) -> String {
        match detect_content_kind(content) {
            ContentKind::Json => compact_json(content),
            ContentKind::Text => {
                let lines: Vec<&str> = content.lines().collect();
                if lines.len() > HEAD_LINES + TAIL_LINES {
                    compact_text(content)
                } else {
                    compact_fallback(content)
                }
            }
        }
    }
}

#[async_trait]
impl MiddlewareDriver for ToolResultCompactionMiddleware {
    async fn on_processing_tool_calls(
        &self,
        state: ExecutionState,
        _runtime: Option<&Arc<SessionRuntime>>,
    ) -> Result<ExecutionState> {
        if !self.active.load(Ordering::Relaxed) {
            return Ok(state);
        }

        let ExecutionState::ProcessingToolCalls {
            remaining_calls,
            results,
            context,
        } = state
        else {
            return Ok(state);
        };

        let span = info_span!(
            "compaction.process",
            results_count = results.len(),
            compacted = tracing::field::Empty,
            skipped = tracing::field::Empty,
            bytes_saved = tracing::field::Empty,
        );
        let _guard = span.enter();

        let mut modified_results: Vec<ToolResult> = results.to_vec();
        let mut any_modified = false;
        let mut batch_compacted: u64 = 0;
        let mut batch_skipped: u64 = 0;
        let mut batch_bytes_saved: i64 = 0;

        for result in &mut modified_results {
            let tool_name = result.tool_name.as_deref().unwrap_or("unknown");

            match self.should_compact(result) {
                Ok(()) => {
                    let original_len = result.content.len();
                    let content_kind = detect_content_kind(&result.content);
                    let compacted = self.compact_result(&result.content);
                    let compacted_len = compacted.len();

                    // Only apply if we actually reduced size
                    if compacted_len < original_len {
                        debug!(
                            tool_name,
                            call_id = %result.call_id,
                            original_bytes = original_len,
                            compacted_bytes = compacted_len,
                            content_kind = ?content_kind,
                            compression_pct = format_args!("{:.1}", (compacted_len as f64 / original_len as f64) * 100.0),
                            "tool result compacted"
                        );

                        result.content = compacted;
                        self.metrics.record_compaction(original_len, compacted_len);
                        any_modified = true;
                        batch_compacted += 1;
                        batch_bytes_saved += (original_len - compacted_len) as i64;
                    } else {
                        trace!(
                            tool_name,
                            call_id = %result.call_id,
                            "compaction would not reduce size, skipping"
                        );
                        self.metrics.record_skip();
                        batch_skipped += 1;
                    }
                }
                Err(reason) => {
                    trace!(
                        tool_name,
                        call_id = %result.call_id,
                        skip_reason = %reason,
                        "tool result compaction skipped"
                    );
                    self.metrics.record_skip();
                    batch_skipped += 1;
                }
            }
        }

        // Record batch-level summary in the span
        span.record("compacted", batch_compacted);
        span.record("skipped", batch_skipped);
        span.record("bytes_saved", batch_bytes_saved);

        // Emit an info-level summary when any compaction occurred
        if batch_compacted > 0 {
            let snap = self.metrics.snapshot();
            info!(
                compacted = batch_compacted,
                skipped = batch_skipped,
                bytes_saved = batch_bytes_saved,
                cumulative_compacted = snap.compacted_count,
                cumulative_bytes_before = snap.bytes_before,
                cumulative_bytes_after = snap.bytes_after,
                cumulative_ratio = format_args!("{:.2}", snap.compression_ratio()),
                "tool result compaction batch complete"
            );
        }

        if any_modified {
            Ok(ExecutionState::ProcessingToolCalls {
                remaining_calls,
                results: modified_results.into(),
                context,
            })
        } else {
            Ok(ExecutionState::ProcessingToolCalls {
                remaining_calls,
                results,
                context,
            })
        }
    }

    fn reset(&self) {
        // Metrics are cumulative per session, no reset needed.
    }

    fn name(&self) -> &'static str {
        "tool_result_compaction"
    }
}

// ============================================================================
// Factory for config-based creation
// ============================================================================

/// Factory for creating `ToolResultCompactionMiddleware` from config.
pub struct ToolResultCompactionFactory;

impl crate::middleware::factory::MiddlewareFactory for ToolResultCompactionFactory {
    fn type_name(&self) -> &'static str {
        "tool_result_compaction"
    }

    fn create(
        &self,
        _config: &serde_json::Value,
        agent_config: &crate::agent::agent_config::AgentConfig,
    ) -> anyhow::Result<Arc<dyn MiddlewareDriver>> {
        let compaction_config = agent_config.execution_policy.tool_result_compaction.clone();

        if !compaction_config.enabled {
            return Err(anyhow::anyhow!(
                "tool_result_compaction middleware is disabled (enabled = false)"
            ));
        }

        Ok(Arc::new(ToolResultCompactionMiddleware::new(
            compaction_config,
        )))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolResultCompactionConfig;
    use crate::middleware::state::{AgentStats, ConversationContext, ToolCall, ToolFunction};

    fn make_config(
        enabled: bool,
        tools: Vec<&str>,
        min_bytes: usize,
    ) -> ToolResultCompactionConfig {
        ToolResultCompactionConfig {
            enabled,
            opt_in_tools: tools.into_iter().map(|s| s.to_string()).collect(),
            min_bytes,
            ..Default::default()
        }
    }

    fn make_tool_result(
        call_id: &str,
        tool_name: &str,
        content: &str,
        is_error: bool,
    ) -> ToolResult {
        ToolResult::new(
            call_id.to_string(),
            content.to_string(),
            is_error,
            Some(tool_name.to_string()),
            Some("{}".to_string()),
        )
    }

    fn make_context() -> Arc<ConversationContext> {
        Arc::new(ConversationContext::new(
            "test".into(),
            Arc::from([]),
            Arc::new(AgentStats::default()),
            "mock".into(),
            "mock-model".into(),
        ))
    }

    fn make_tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            function: ToolFunction {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    // ── should_compact gating tests ────────────────────────────────────────

    #[test]
    fn test_should_compact_disabled() {
        let mw = ToolResultCompactionMiddleware::new(make_config(false, vec!["shell"], 100));
        let result = make_tool_result("c1", "shell", "x".repeat(200).as_str(), false);
        assert!(matches!(
            mw.should_compact(&result),
            Err(SkipReason::FeatureDisabled)
        ));
    }

    #[test]
    fn test_should_compact_error_result() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));
        let result = make_tool_result("c1", "shell", &"x".repeat(200), true);
        assert!(matches!(
            mw.should_compact(&result),
            Err(SkipReason::IsError)
        ));
    }

    #[test]
    fn test_should_compact_not_opt_in() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));
        let result = make_tool_result("c1", "read_tool", &"x".repeat(200), false);
        assert!(matches!(
            mw.should_compact(&result),
            Err(SkipReason::NotOptIn)
        ));
    }

    #[test]
    fn test_should_compact_below_threshold() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 1000));
        let result = make_tool_result("c1", "shell", "small output", false);
        assert!(matches!(
            mw.should_compact(&result),
            Err(SkipReason::BelowThreshold)
        ));
    }

    #[test]
    fn test_should_compact_passes() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));
        let result = make_tool_result("c1", "shell", &"x".repeat(200), false);
        assert!(mw.should_compact(&result).is_ok());
    }

    // ── Content detection tests ────────────────────────────────────────────

    #[test]
    fn test_detect_json_object() {
        assert_eq!(
            detect_content_kind("{\"key\": \"value\"}"),
            ContentKind::Json
        );
    }

    #[test]
    fn test_detect_json_array() {
        assert_eq!(detect_content_kind("[1, 2, 3]"), ContentKind::Json);
    }

    #[test]
    fn test_detect_text() {
        assert_eq!(
            detect_content_kind("hello world\nline 2"),
            ContentKind::Text
        );
    }

    #[test]
    fn test_detect_invalid_json_is_text() {
        assert_eq!(detect_content_kind("{not json at all"), ContentKind::Text);
    }

    // ── Text compaction tests ──────────────────────────────────────────────

    #[test]
    fn test_compact_text_preserves_small_output() {
        let content = "line 1\nline 2\nline 3\n";
        let result = compact_text(content);
        // Small output should not be wrapped in compaction markers
        // (goes through compact_long_lines which passes through if under char limit)
        assert!(!result.contains("[Compacted:"));
    }

    #[test]
    fn test_compact_text_large_output() {
        let mut content = String::new();
        for i in 0..300 {
            content.push_str(&format!("line {i}: some log output here\n"));
        }
        let result = compact_text(&content);
        assert!(result.contains("[Compacted:"));
        assert!(result.contains("HEAD"));
        assert!(result.contains("TAIL"));
        assert!(result.contains("retrieval"));
        // Should be significantly smaller
        assert!(result.len() < content.len());
    }

    #[test]
    fn test_compact_text_preserves_errors() {
        let mut content = String::new();
        for i in 0..300 {
            if i == 150 {
                content.push_str("error: something went wrong\n");
            } else if i == 200 {
                content.push_str("warning: be careful\n");
            } else {
                content.push_str(&format!("line {i}\n"));
            }
        }
        let result = compact_text(&content);
        assert!(result.contains("IMPORTANT"));
        assert!(result.contains("error: something went wrong"));
        assert!(result.contains("warning: be careful"));
    }

    #[test]
    fn test_dedup_consecutive() {
        let lines = vec!["line 1", "repeated", "repeated", "repeated", "line 5"];
        let result = dedup_consecutive(&lines);
        assert!(result.contains("repeated"));
        assert!(result.contains("repeated 2 more times"));
    }

    // ── JSON compaction tests ──────────────────────────────────────────────

    #[test]
    fn test_compact_json_object() {
        let json = serde_json::json!({
            "name": "test",
            "count": 42,
            "active": true,
            "items": [1, 2, 3, 4, 5],
            "nested": {"a": 1, "b": 2}
        });
        let content = serde_json::to_string_pretty(&json).unwrap();
        let result = compact_json(&content);
        assert!(result.contains("[Compacted JSON:"));
        assert!(result.contains("\"name\""));
        assert!(result.contains("\"count\""));
        assert!(result.contains("array(len=5)"));
    }

    #[test]
    fn test_compact_json_array() {
        let items: Vec<serde_json::Value> = (0..100)
            .map(|i| serde_json::json!({"id": i, "name": format!("item-{i}")}))
            .collect();
        let content = serde_json::to_string_pretty(&items).unwrap();
        let result = compact_json(&content);
        assert!(result.contains("Array with 100 elements"));
        assert!(result.contains("[0]:"));
        // Should only show MAX_JSON_SAMPLES
        assert!(result.contains("more elements"));
    }

    // ── Middleware integration tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_middleware_passthrough_when_disabled() {
        let mw = ToolResultCompactionMiddleware::new(make_config(false, vec!["shell"], 100));

        let big_output = "x".repeat(500);
        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![make_tool_call("c1", "shell")].into(),
            results: vec![make_tool_result("c1", "shell", &big_output, false)].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            assert_eq!(results[0].content, big_output, "should not be modified");
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_middleware_compacts_large_text() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));

        let mut big_output = String::new();
        for i in 0..300 {
            big_output.push_str(&format!("line {i}: log output\n"));
        }

        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![].into(),
            results: vec![make_tool_result("c1", "shell", &big_output, false)].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            assert!(
                results[0].content.len() < big_output.len(),
                "compacted output should be smaller"
            );
            assert!(results[0].content.contains("[Compacted:"));
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_middleware_skips_non_opt_in_tools() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));

        let big_output = "x".repeat(500);
        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![].into(),
            results: vec![make_tool_result("c1", "read_tool", &big_output, false)].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            assert_eq!(
                results[0].content, big_output,
                "non-opt-in should not be modified"
            );
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_middleware_skips_error_results() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));

        let big_output = "x".repeat(500);
        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![].into(),
            results: vec![make_tool_result("c1", "shell", &big_output, true)].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            assert_eq!(
                results[0].content, big_output,
                "errors should not be modified"
            );
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_middleware_skips_below_threshold() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 10000));

        let small_output = "small output";
        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![].into(),
            results: vec![make_tool_result("c1", "shell", small_output, false)].into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            assert_eq!(
                results[0].content, small_output,
                "below threshold should not be modified"
            );
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[tokio::test]
    async fn test_middleware_non_processing_state_passthrough() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));
        let state = ExecutionState::Complete;
        let result = mw.on_processing_tool_calls(state, None).await.unwrap();
        assert!(matches!(result, ExecutionState::Complete));
    }

    #[tokio::test]
    async fn test_metrics_tracking() {
        let mw = ToolResultCompactionMiddleware::new(make_config(true, vec!["shell"], 100));

        let mut big_output = String::new();
        for i in 0..300 {
            big_output.push_str(&format!("line {i}: log output\n"));
        }

        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![].into(),
            results: vec![
                make_tool_result("c1", "shell", &big_output, false),
                make_tool_result("c2", "read_tool", &big_output, false), // not opt-in
            ]
            .into(),
            context: make_context(),
        };

        let _ = mw.on_processing_tool_calls(state, None).await.unwrap();

        let snapshot = mw.metrics.snapshot();
        assert_eq!(
            snapshot.compacted_count, 1,
            "one result should be compacted"
        );
        assert_eq!(snapshot.skipped_count, 1, "one result should be skipped");
        assert!(snapshot.bytes_before > 0);
        assert!(snapshot.bytes_after > 0);
        assert!(snapshot.bytes_after < snapshot.bytes_before);
    }

    #[tokio::test]
    async fn test_multiple_results_mixed() {
        let mw =
            ToolResultCompactionMiddleware::new(make_config(true, vec!["shell", "web_fetch"], 100));

        let mut big_text = String::new();
        for i in 0..300 {
            big_text.push_str(&format!("line {i}\n"));
        }

        let big_json = serde_json::to_string_pretty(
            &(0..50)
                .map(|i| serde_json::json!({"id": i}))
                .collect::<Vec<_>>(),
        )
        .unwrap();

        let state = ExecutionState::ProcessingToolCalls {
            remaining_calls: vec![].into(),
            results: vec![
                make_tool_result("c1", "shell", &big_text, false), // compacted (text)
                make_tool_result("c2", "web_fetch", &big_json, false), // compacted (json)
                make_tool_result("c3", "read_tool", &big_text, false), // skipped (not opt-in)
                make_tool_result("c4", "shell", "small", false),   // skipped (below threshold)
                make_tool_result("c5", "shell", &big_text, true),  // skipped (error)
            ]
            .into(),
            context: make_context(),
        };

        let result = mw.on_processing_tool_calls(state, None).await.unwrap();

        if let ExecutionState::ProcessingToolCalls { results, .. } = result {
            // c1: compacted text
            assert!(results[0].content.contains("[Compacted:"));
            // c2: compacted json
            assert!(results[1].content.contains("[Compacted JSON:"));
            // c3: unchanged (not opt-in)
            assert_eq!(results[2].content, big_text);
            // c4: unchanged (below threshold)
            assert_eq!(results[3].content, "small");
            // c5: unchanged (error)
            assert_eq!(results[4].content, big_text);
        } else {
            panic!("expected ProcessingToolCalls state");
        }
    }

    #[test]
    fn test_compression_ratio() {
        let snap = CompactionMetricsSnapshot {
            compacted_count: 2,
            skipped_count: 1,
            bytes_before: 10000,
            bytes_after: 2000,
        };
        assert!((snap.compression_ratio() - 0.2).abs() < 0.001);
    }

    #[test]
    fn test_compression_ratio_zero_bytes() {
        let snap = CompactionMetricsSnapshot {
            compacted_count: 0,
            skipped_count: 0,
            bytes_before: 0,
            bytes_after: 0,
        };
        assert!((snap.compression_ratio() - 1.0).abs() < 0.001);
    }
}
