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
