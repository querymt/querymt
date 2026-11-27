//! Projection stores for role-based observability views.
//!
//! This module provides different views of session data:
//! - Full audit view (internal/compliance)
//! - Redacted view (user-facing, sensitive data removed)
//! - Summary view (quick status for UI)

use crate::events::{AgentEventKind, DurableEvent, EventOrigin};
use querymt::chat::FinishReason;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use typeshare::typeshare;

pub use crate::session::event_journal::EventJournal;
pub use crate::session::view_store::ViewStore;
pub use querymt_agent_protocol_types::session::views::{AuditView, SessionWorkspaceView};

/// Redacted view for user-facing display (sensitive data removed)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedView {
    pub session_id: String,
    pub current_intent: Option<String>,
    pub active_task: Option<RedactedTask>,
    pub recent_progress: Vec<RedactedProgress>,
    pub artifacts: Vec<RedactedArtifact>,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedTask {
    pub id: String,
    pub status: String,
    pub revision: u64,
    pub current_task: bool,
    pub expected_deliverable: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedProgress {
    pub kind: String,
    pub summary: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedArtifact {
    pub kind: String,
    pub summary: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Summary view for quick status display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryView {
    pub session_id: String,
    pub current_intent: Option<String>,
    pub active_task_status: Option<String>,
    pub progress_count: usize,
    pub artifact_count: usize,
    pub decision_count: usize,
    pub last_activity: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Default)]
pub struct SessionListMetaStats {
    pub message_count: u32,
    pub user_message_count: u32,
    pub last_finish_reason: Option<FinishReason>,
}

/// Redaction policy for controlling what information is shown
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionPolicy {
    /// Show everything (internal use)
    None,
    /// Hide sensitive tool arguments and results
    Sensitive,
    /// Show only high-level summaries
    Minimal,
}

// ============================================================================
// EventJournal — durable event persistence trait
// ============================================================================

/// Input for appending a new durable event to the journal.
#[derive(Debug, Clone)]
pub struct NewDurableEvent {
    pub session_id: String,
    pub origin: EventOrigin,
    /// Display-only source label (hostname/peer label); never used as a
    /// deduplication key (plan §15: stable node ID only).
    pub source_node: Option<String>,
    /// Stable remote node identity (bookmark `node_id`). `None` for locally
    /// produced events. Together with `source_seq` this forms the remote
    /// event uniqueness key `(session_id, source_node_id, source_seq)`.
    pub source_node_id: Option<String>,
    /// Source-side journal sequence (the host's `stream_seq` for the event).
    /// `None` for locally produced events and for relayed events that carry
    /// no usable source cursor.
    pub source_seq: Option<i64>,
    /// Producer-assigned per-session sequence (see `DurableEvent::session_seq`).
    /// For relayed events this is the host's value. `None` leaves the column
    /// unset (annotation events and callers outside a session sequencer).
    pub session_seq: Option<i64>,
    pub kind: AgentEventKind,
}

/// Direction of an [`EventJournal::load_history_page`] read. Positions are
/// exclusive bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryPageDirection {
    /// The first events with position `> after`.
    Forward { after: i64 },
    /// The newest events with position `< before` (`None` = up to the tip).
    Backward { before: Option<i64> },
}

/// Select one page out of position-ordered history (ascending result).
#[doc(hidden)]
pub fn slice_history_page(
    mut history: Vec<(i64, DurableEvent)>,
    direction: HistoryPageDirection,
    limit: usize,
) -> Vec<(i64, DurableEvent)> {
    match direction {
        HistoryPageDirection::Forward { after } => {
            history.retain(|(position, _)| *position > after);
            history.truncate(limit);
            history
        }
        HistoryPageDirection::Backward { before } => {
            history.retain(|(position, _)| before.is_none_or(|before| *position < before));
            let skip = history.len().saturating_sub(limit);
            history.split_off(skip)
        }
    }
}

/// Run lifecycle marker used by [`last_unfinished_run_in`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub enum RunLifecycle {
    Started,
    Completed,
}

/// Fold run lifecycle events (in journal order) to the latest run that was
/// started and never completed.
#[doc(hidden)]
pub fn last_unfinished_run_in(
    events: impl IntoIterator<Item = (RunLifecycle, String)>,
) -> Option<String> {
    let mut open: Vec<String> = Vec::new();
    for (lifecycle, run_id) in events {
        match lifecycle {
            RunLifecycle::Started => open.push(run_id),
            RunLifecycle::Completed => open.retain(|id| *id != run_id),
        }
    }
    open.pop()
}

/// Upper bound on one flat `list_session_items*` page; the ACP global page size must fit under it.
pub const MAX_SESSION_ITEMS_PAGE: usize = 500;

/// Helper trait for redacting sensitive content
pub trait Redactor {
    /// Redact a string based on policy
    fn redact(&self, content: &str, policy: RedactionPolicy) -> String;

    /// Check if a field should be included based on policy
    fn should_include(&self, field_sensitivity: FieldSensitivity, policy: RedactionPolicy) -> bool;
}

/// Field sensitivity classification
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FieldSensitivity {
    /// Public information
    Public,
    /// Internal details
    Internal,
    /// Sensitive data (credentials, PII, etc.)
    Sensitive,
}

/// Default redactor implementation
pub struct DefaultRedactor;

impl Redactor for DefaultRedactor {
    fn redact(&self, content: &str, policy: RedactionPolicy) -> String {
        match policy {
            RedactionPolicy::None => content.to_string(),
            RedactionPolicy::Sensitive => {
                // Simple redaction: replace with placeholder if looks sensitive
                if content.contains("password")
                    || content.contains("token")
                    || content.contains("secret")
                    || content.contains("api_key")
                {
                    "[REDACTED]".to_string()
                } else {
                    content.to_string()
                }
            }
            RedactionPolicy::Minimal => {
                // Only show first 100 chars as summary
                truncate_with_ellipsis(content, 100)
            }
        }
    }

    fn should_include(&self, field_sensitivity: FieldSensitivity, policy: RedactionPolicy) -> bool {
        match policy {
            RedactionPolicy::None => true,
            RedactionPolicy::Sensitive => field_sensitivity < FieldSensitivity::Sensitive,
            RedactionPolicy::Minimal => field_sensitivity == FieldSensitivity::Public,
        }
    }
}

// ============================================================================
// Session List View - for UI splash screen / session picker
// ============================================================================

/// Generic field predicate for filtering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldPredicate {
    pub field: String,
    pub op: PredicateOp,
}

/// Predicate operations for filtering
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", content = "value", rename_all = "snake_case")]
pub enum PredicateOp {
    Eq(serde_json::Value),
    Ne(serde_json::Value),
    Gt(serde_json::Value),
    Gte(serde_json::Value),
    Lt(serde_json::Value),
    Lte(serde_json::Value),
    Contains(String),
    StartsWith(String),
    IsNull,
    IsNotNull,
    In(Vec<serde_json::Value>),
}

/// Filter expression with boolean logic
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FilterExpr {
    Predicate(FieldPredicate),
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
}

/// Filter for session list queries
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionListFilter {
    pub filter: Option<FilterExpr>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[typeshare]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionScope {
    #[default]
    All,
    Root,
    Forks,
    Delegates,
    Children,
}

impl SessionScope {
    pub fn from_option(value: Option<&str>) -> Self {
        match value {
            Some("root") => Self::Root,
            Some("forks") => Self::Forks,
            Some("delegates") => Self::Delegates,
            Some("children") => Self::Children,
            _ => Self::All,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListItem {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<SessionWorkspaceView>,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    /// Public ID of the parent session (if this is a child session)
    pub parent_session_id: Option<String>,
    /// Fork origin: "user" or "delegation"
    pub fork_origin: Option<String>,
    /// Session kind (e.g. "recurring") for specialized UI badges.
    pub session_kind: Option<String>,
    /// Whether this session has child sessions
    pub has_children: bool,
    pub fork_count: usize,
    /// A persisted fork-isolation intent is still outstanding: the fork is
    /// not runnable (load/resume fail closed) until explicit recovery
    /// completes provisioning. Set for pending and failed isolated fork
    /// creation alike; the workspace view, when present, carries the
    /// workspace-level lifecycle (e.g. `failed`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending_isolation: bool,
    /// Entries of the session's current todo plan, as a `plan_update` for it
    /// would carry them. `None` when the session has no (or an emptied) todo
    /// list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_plan: Option<Vec<crate::protocol::PlanEntry>>,
}

impl SessionListItem {
    /// Managed sessions group under the cwd their workspace was created
    /// from, local ones under their own cwd.
    pub fn group_key(&self) -> Option<String> {
        self.workspace
            .as_ref()
            .map(|w| w.source_cwd.as_str())
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_owned)
            .or_else(|| self.cwd.clone())
    }
}

/// Group of sessions by project cwd (the source cwd for managed sessions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub cwd: Option<String>,
    pub sessions: Vec<SessionListItem>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub latest_activity: Option<OffsetDateTime>,
    pub total_count: Option<usize>,
    pub next_cursor: Option<String>,
}

/// Session list view with grouping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListView {
    pub groups: Vec<SessionGroup>,
    pub total_count: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroupTotal {
    pub cwd: Option<String>,
    pub total_count: usize,
    #[serde(with = "time::serde::rfc3339::option")]
    pub latest_activity: Option<OffsetDateTime>,
}

/// Recent model usage entry from event history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentModelEntry {
    pub provider: String,
    pub model: String,
    #[serde(with = "time::serde::rfc3339")]
    pub last_used: OffsetDateTime,
    pub use_count: u32,
}

/// Recent models view grouped by workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentModelsView {
    /// Models grouped by workspace (cwd path string, or None for no-workspace sessions)
    pub by_workspace: std::collections::HashMap<Option<String>, Vec<RecentModelEntry>>,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

/// Truncate `input` to at most `max_bytes` bytes, appending `"..."` when it is
/// shortened. Always cuts on a UTF-8 char boundary.
///
/// A copy of `querymt_utils::str_utils::truncate_with_ellipsis`: the
/// core layer must not depend on `querymt-utils`.
#[doc(hidden)]
pub fn truncate_with_ellipsis(input: &str, max_bytes: usize) -> String {
    const ELLIPSIS: &str = "...";
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes <= ELLIPSIS.len() {
        // Not enough room for text + "...": return as many whole chars as fit.
        return input.chars().take(max_bytes).collect();
    }
    let mut end = max_bytes - ELLIPSIS.len();
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &input[..end], ELLIPSIS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    // ── RedactionPolicy ──────────────────────────────────────────────────────

    #[test]
    fn test_redaction_policy_variants_compare() {
        assert_eq!(RedactionPolicy::None, RedactionPolicy::None);
        assert_ne!(RedactionPolicy::None, RedactionPolicy::Sensitive);
        assert_ne!(RedactionPolicy::Sensitive, RedactionPolicy::Minimal);
    }

    // ── DefaultRedactor ──────────────────────────────────────────────────────

    #[test]
    fn test_default_redactor_none_policy_passes_through() {
        let r = DefaultRedactor;
        assert_eq!(
            r.redact("hello world", RedactionPolicy::None),
            "hello world"
        );
    }

    #[test]
    fn test_default_redactor_sensitive_redacts_keywords() {
        let r = DefaultRedactor;
        assert_eq!(
            r.redact("my password is 123", RedactionPolicy::Sensitive),
            "[REDACTED]"
        );
        assert_eq!(
            r.redact("token: abc123", RedactionPolicy::Sensitive),
            "[REDACTED]"
        );
        assert_eq!(
            r.redact("api_key=xyz", RedactionPolicy::Sensitive),
            "[REDACTED]"
        );
        assert_eq!(
            r.redact("secret stuff", RedactionPolicy::Sensitive),
            "[REDACTED]"
        );
    }

    #[test]
    fn test_default_redactor_sensitive_passes_non_sensitive() {
        let r = DefaultRedactor;
        let safe = "this is fine content";
        assert_eq!(r.redact(safe, RedactionPolicy::Sensitive), safe);
    }

    #[test]
    fn test_default_redactor_minimal_truncates_long_content() {
        let r = DefaultRedactor;
        let long_content = "a".repeat(200);
        let result = r.redact(&long_content, RedactionPolicy::Minimal);
        assert!(result.ends_with("..."));
        assert!(result.len() <= 103);
    }

    #[test]
    fn test_default_redactor_minimal_passes_short_content() {
        let r = DefaultRedactor;
        let short = "short text";
        assert_eq!(r.redact(short, RedactionPolicy::Minimal), short);
    }

    #[test]
    fn test_default_redactor_should_include_none_policy() {
        let r = DefaultRedactor;
        assert!(r.should_include(FieldSensitivity::Public, RedactionPolicy::None));
        assert!(r.should_include(FieldSensitivity::Internal, RedactionPolicy::None));
        assert!(r.should_include(FieldSensitivity::Sensitive, RedactionPolicy::None));
    }

    #[test]
    fn test_default_redactor_should_include_sensitive_policy() {
        let r = DefaultRedactor;
        assert!(r.should_include(FieldSensitivity::Public, RedactionPolicy::Sensitive));
        assert!(r.should_include(FieldSensitivity::Internal, RedactionPolicy::Sensitive));
        assert!(!r.should_include(FieldSensitivity::Sensitive, RedactionPolicy::Sensitive));
    }

    #[test]
    fn test_default_redactor_should_include_minimal_policy() {
        let r = DefaultRedactor;
        assert!(r.should_include(FieldSensitivity::Public, RedactionPolicy::Minimal));
        assert!(!r.should_include(FieldSensitivity::Internal, RedactionPolicy::Minimal));
        assert!(!r.should_include(FieldSensitivity::Sensitive, RedactionPolicy::Minimal));
    }

    // ── FieldSensitivity ordering ─────────────────────────────────────────────

    #[test]
    fn test_field_sensitivity_ordering() {
        assert!(FieldSensitivity::Public < FieldSensitivity::Internal);
        assert!(FieldSensitivity::Internal < FieldSensitivity::Sensitive);
        assert!(FieldSensitivity::Public < FieldSensitivity::Sensitive);
    }

    // ── View struct construction ──────────────────────────────────────────────
    // NOTE: AuditView is intentionally not tested here.
    // AuditView contains Vec<AgentEvent> whose serde impl chain (30+ variant enum
    // with deeply nested domain types) overflows the rustc trait-solver (E0275)
    // when evaluated in a lib-test compilation unit.  Coverage via integration tests.

    #[test]
    fn test_summary_view_construction() {
        let view = SummaryView {
            session_id: "sess-2".to_string(),
            current_intent: Some("Build X".to_string()),
            active_task_status: Some("active".to_string()),
            progress_count: 5,
            artifact_count: 2,
            decision_count: 1,
            last_activity: None,
            generated_at: now(),
        };
        assert_eq!(view.session_id, "sess-2");
        assert_eq!(view.progress_count, 5);
        assert_eq!(view.artifact_count, 2);
    }

    #[test]
    fn test_session_list_filter_default() {
        let filter = SessionListFilter::default();
        assert!(filter.filter.is_none());
        assert!(filter.limit.is_none());
        assert!(filter.offset.is_none());
    }

    #[test]
    fn test_session_scope_from_option_defaults_and_parses_known_values() {
        assert_eq!(SessionScope::from_option(None), SessionScope::All);
        assert_eq!(SessionScope::from_option(Some("all")), SessionScope::All);
        assert_eq!(SessionScope::from_option(Some("root")), SessionScope::Root);
        assert_eq!(
            SessionScope::from_option(Some("forks")),
            SessionScope::Forks
        );
        assert_eq!(
            SessionScope::from_option(Some("delegates")),
            SessionScope::Delegates
        );
        assert_eq!(
            SessionScope::from_option(Some("children")),
            SessionScope::Children
        );
        assert_eq!(
            SessionScope::from_option(Some("unknown")),
            SessionScope::All
        );
    }

    #[test]
    fn test_session_scope_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionScope::Root).unwrap(),
            "\"root\""
        );
        assert_eq!(
            serde_json::to_string(&SessionScope::Delegates).unwrap(),
            "\"delegates\""
        );
        assert_eq!(
            serde_json::from_str::<SessionScope>("\"children\"").unwrap(),
            SessionScope::Children
        );
    }

    #[test]
    fn test_session_list_item_construction() {
        let item = SessionListItem {
            session_id: "sess-x".to_string(),
            workspace: None,
            name: Some("My Session".to_string()),
            cwd: Some("/home/user/project".to_string()),
            title: None,
            created_at: Some(now()),
            updated_at: None,
            parent_session_id: None,
            fork_origin: None,
            session_kind: None,
            has_children: false,
            fork_count: 0,
            pending_isolation: false,
            todo_plan: None,
        };
        assert_eq!(item.session_id, "sess-x");
        assert!(!item.has_children);
        assert_eq!(item.fork_count, 0);
    }

    #[test]
    fn test_session_list_view_construction() {
        let view = SessionListView {
            groups: vec![],
            total_count: 0,
            generated_at: now(),
        };
        assert_eq!(view.total_count, 0);
        assert!(view.groups.is_empty());
    }

    // ── PredicateOp serialization ─────────────────────────────────────────────
    // NOTE: FilterExpr is a recursive type (And(Vec<FilterExpr>), Not(Box<FilterExpr>)).
    // Calling serde_json::to_string on FilterExpr overflows the rustc trait-solver
    // (E0275) due to infinite recursion in Serialize bound evaluation.
    // We test construction and field access only; FilterExpr serialization is a
    // property of the derive macro, not our logic.

    #[test]
    fn test_predicate_op_serialization() {
        let eq_op = PredicateOp::Eq(serde_json::Value::String("test".to_string()));
        let json = serde_json::to_string(&eq_op).unwrap();
        assert!(json.contains("eq"));

        let contains_op = PredicateOp::Contains("hello".to_string());
        let json2 = serde_json::to_string(&contains_op).unwrap();
        assert!(json2.contains("contains"));
    }

    #[test]
    fn test_filter_expr_construction() {
        // Construction only — no serde_json::to_string (see note above).
        let expr = FilterExpr::And(vec![
            FilterExpr::Predicate(FieldPredicate {
                field: "status".to_string(),
                op: PredicateOp::Eq(serde_json::json!("active")),
            }),
            FilterExpr::Not(Box::new(FilterExpr::Predicate(FieldPredicate {
                field: "name".to_string(),
                op: PredicateOp::IsNull,
            }))),
        ]);
        // Verify the structure is as expected
        if let FilterExpr::And(items) = &expr {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected FilterExpr::And");
        }
    }

    #[test]
    fn test_redacted_view_construction() {
        let view = RedactedView {
            session_id: "sess-r".to_string(),
            current_intent: Some("doing X".to_string()),
            active_task: Some(RedactedTask {
                id: "task-1".to_string(),
                status: "Active".to_string(),
                revision: 1,
                current_task: true,
                expected_deliverable: Some("deliver".to_string()),
            }),
            recent_progress: vec![RedactedProgress {
                kind: "tool_call".to_string(),
                summary: "ran shell".to_string(),
                created_at: now(),
            }],
            artifacts: vec![],
            generated_at: now(),
        };
        assert_eq!(view.session_id, "sess-r");
        assert!(view.active_task.is_some());
        assert_eq!(view.recent_progress.len(), 1);
    }

    #[test]
    fn truncate_with_ellipsis_respects_char_boundaries() {
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_with_ellipsis("hello world", 8), "hello...");
        assert_eq!(truncate_with_ellipsis("abc", 1), "a");
        // Each Cyrillic char is 2 bytes: 7 bytes leaves room for 2 chars + "...".
        assert_eq!(truncate_with_ellipsis("абвгдежз", 7), "аб...");
        // Budget lands inside a multibyte char: walk back to the boundary.
        let out = truncate_with_ellipsis("абвгдежз", 8);
        assert!(out.ends_with("..."));
        assert!(out.len() <= 8);
    }
}
