//! Projection stores for role-based observability views.
//!
//! This module provides different views of session data:
//! - Full audit view (internal/compliance)
//! - Redacted view (user-facing, sensitive data removed)
//! - Summary view (quick status for UI)

use crate::events::AgentEvent;
use crate::session::error::SessionResult;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::domain::{Artifact, Decision, Delegation, IntentSnapshot, ProgressEntry, Task};

/// Full audit view with all details for internal/compliance use
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditView {
    pub session_id: String,
    pub events: Vec<AgentEvent>,
    pub tasks: Vec<Task>,
    pub intent_snapshots: Vec<IntentSnapshot>,
    pub decisions: Vec<Decision>,
    pub progress_entries: Vec<ProgressEntry>,
    pub artifacts: Vec<Artifact>,
    pub delegations: Vec<Delegation>,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
}

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

/// Event persistence and querying
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Append an event to the event log
    async fn append_event(&self, event: &AgentEvent) -> SessionResult<()>;

    /// Get all events for a session
    async fn get_session_events(&self, session_id: &str) -> SessionResult<Vec<AgentEvent>>;

    /// Get events since a specific sequence number
    async fn get_events_since(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> SessionResult<Vec<AgentEvent>>;
}

/// View generation (read-only projections)
#[async_trait]
pub trait ViewStore: Send + Sync {
    /// Generate a full audit view for a session
    ///
    /// # Arguments
    /// * `session_id` - The session to get the audit view for
    /// * `include_children` - Whether to include child session events (e.g., delegations)
    ///   Set to `true` for complete trajectory exports, `false` for UI rendering
    ///   (UI subscribes to child sessions separately)
    async fn get_audit_view(
        &self,
        session_id: &str,
        include_children: bool,
    ) -> SessionResult<AuditView>;

    /// Generate a redacted view for a session
    async fn get_redacted_view(
        &self,
        session_id: &str,
        policy: RedactionPolicy,
    ) -> SessionResult<RedactedView>;

    /// Generate a summary view for a session
    async fn get_summary_view(&self, session_id: &str) -> SessionResult<SummaryView>;

    /// Generate a session list view with optional filtering
    /// If filter is None, returns all sessions (up to default limit)
    /// Sessions are grouped by CWD and sorted by latest activity
    async fn get_session_list_view(
        &self,
        filter: Option<SessionListFilter>,
    ) -> SessionResult<SessionListView>;

    /// Export session as ATIF (Agent Trajectory Interchange Format)
    async fn get_atif(
        &self,
        session_id: &str,
        options: &crate::export::AtifExportOptions,
    ) -> SessionResult<crate::export::ATIF>;

    /// Get recent models from full event history, grouped by workspace
    /// Returns all models ever used, sorted by last_used descending
    ///
    /// # Arguments
    /// * `limit_per_workspace` - Maximum number of recent models to return per workspace
    async fn get_recent_models_view(
        &self,
        limit_per_workspace: usize,
    ) -> SessionResult<RecentModelsView>;
}

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
                if content.len() > 100 {
                    format!("{}...", &content[..100])
                } else {
                    content.to_string()
                }
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

/// Individual session item for list display
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListItem {
    pub session_id: String,
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
    /// Whether this session has child sessions
    pub has_children: bool,
}

/// Group of sessions by CWD
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub cwd: Option<String>,
    pub sessions: Vec<SessionListItem>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub latest_activity: Option<OffsetDateTime>,
}

/// Session list view with grouping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListView {
    pub groups: Vec<SessionGroup>,
    pub total_count: usize,
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
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
    fn test_session_list_item_construction() {
        let item = SessionListItem {
            session_id: "sess-x".to_string(),
            name: Some("My Session".to_string()),
            cwd: Some("/home/user/project".to_string()),
            title: None,
            created_at: Some(now()),
            updated_at: None,
            parent_session_id: None,
            fork_origin: None,
            has_children: false,
        };
        assert_eq!(item.session_id, "sess-x");
        assert!(!item.has_children);
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
                status: "active".to_string(),
                expected_deliverable: Some("output.txt".to_string()),
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
}
