//! The `ViewStore` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use super::projection::*;
use crate::session::error::{SessionError, SessionResult};
use crate::session::lifecycle::{SessionLifecycle, SessionLifecycleFilter};
use async_trait::async_trait;
use std::collections::HashMap;

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

    /// The audit view's whole-session fields (tasks, decisions, intents,
    /// progress, artifacts, delegations) with an empty `events` list, for
    /// windowed loads that read events separately. The default derives it
    /// from [`Self::get_audit_view`]; backends should skip the event read.
    async fn get_audit_view_metadata(&self, session_id: &str) -> SessionResult<AuditView> {
        let mut view = self.get_audit_view(session_id, false).await?;
        view.events = Vec::new();
        Ok(view)
    }

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

    /// Browse workspace groups with preview sessions.
    async fn browse_session_groups(
        &self,
        cursor: Option<String>,
        group_limit: usize,
        session_limit_per_group: usize,
        session_scope: SessionScope,
    ) -> SessionResult<(Vec<SessionGroup>, Option<String>, usize)>;

    /// Page sessions for one workspace group.
    async fn list_group_sessions(
        &self,
        cwd: Option<String>,
        cursor: Option<String>,
        limit: usize,
        session_scope: SessionScope,
    ) -> SessionResult<(SessionGroup, usize)>;

    /// Page flat session metadata rows using the same title and hierarchy projection logic
    /// as the UI session list.
    async fn list_session_items(
        &self,
        cwd: Option<String>,
        cursor: Option<String>,
        limit: usize,
        session_scope: SessionScope,
    ) -> SessionResult<(Vec<SessionListItem>, Option<String>, usize)>;

    /// Batch lightweight stats needed by ACP session metadata for a paginated list.
    async fn get_session_list_meta_stats(
        &self,
        session_ids: &[String],
    ) -> SessionResult<HashMap<String, SessionListMetaStats>>;

    /// Search all sessions via FTS5 (DB-wide, not limited to loaded pages).
    async fn search_sessions(
        &self,
        query: String,
        cursor: Option<String>,
        limit: usize,
        session_scope: SessionScope,
    ) -> SessionResult<(Vec<SessionGroup>, Option<String>, usize)>;

    /// Page direct user-fork children for one parent/root session.
    async fn list_session_children(
        &self,
        parent_session_id: String,
        cursor: Option<String>,
        limit: usize,
    ) -> SessionResult<(SessionGroup, usize)>;

    /// Get recent models from full event history, grouped by workspace
    /// Returns all models ever used, sorted by last_used descending
    ///
    /// # Arguments
    /// * `limit_per_workspace` - Maximum number of recent models to return per workspace
    async fn get_recent_models_view(
        &self,
        limit_per_workspace: usize,
    ) -> SessionResult<RecentModelsView>;

    /// Whether this view store can authoritatively provide lifecycle metadata.
    fn supports_session_lifecycle(&self) -> bool {
        false
    }

    /// Whether lifecycle filters are supported across all lifecycle-aware queries.
    fn supports_session_lifecycle_filtering(&self) -> bool {
        false
    }

    /// Batch-fetch lifecycle metadata for listed sessions.
    async fn get_session_list_lifecycles(
        &self,
        _session_ids: &[String],
    ) -> SessionResult<HashMap<String, SessionLifecycle>> {
        Err(SessionError::InvalidOperation(
            "Session lifecycle is unsupported by this view store".into(),
        ))
    }

    /// Current todo plan of each listed session, keyed by session id. A
    /// session without a plan (never had one, or emptied it) is absent from
    /// the map. An `Err` means the store cannot say, which callers must not
    /// read as "no plans".
    async fn get_session_todo_plans(
        &self,
        _session_ids: &[String],
    ) -> SessionResult<HashMap<String, Vec<crate::protocol::PlanEntry>>> {
        Err(SessionError::InvalidOperation(
            "Session todo plans are unsupported by this view store".into(),
        ))
    }

    /// Lifecycle-aware variant of [`ViewStore::get_session_list_view`].
    async fn get_session_list_view_with_lifecycle(
        &self,
        filter: Option<SessionListFilter>,
        lifecycle: SessionLifecycleFilter,
    ) -> SessionResult<SessionListView> {
        if lifecycle.is_filtered() {
            return Err(SessionError::InvalidOperation(
                "Session lifecycle filtering is unsupported by this view store".into(),
            ));
        }
        self.get_session_list_view(filter).await
    }

    /// Lifecycle-aware variant of [`ViewStore::browse_session_groups`].
    async fn browse_session_groups_with_lifecycle(
        &self,
        cursor: Option<String>,
        group_limit: usize,
        session_limit_per_group: usize,
        session_scope: SessionScope,
        lifecycle: SessionLifecycleFilter,
    ) -> SessionResult<(Vec<SessionGroup>, Option<String>, usize)> {
        if lifecycle.is_filtered() {
            return Err(SessionError::InvalidOperation(
                "Session lifecycle filtering is unsupported by this view store".into(),
            ));
        }
        self.browse_session_groups(cursor, group_limit, session_limit_per_group, session_scope)
            .await
    }

    /// Lifecycle-aware variant of [`ViewStore::list_group_sessions`].
    async fn list_group_sessions_with_lifecycle(
        &self,
        cwd: Option<String>,
        cursor: Option<String>,
        limit: usize,
        session_scope: SessionScope,
        lifecycle: SessionLifecycleFilter,
    ) -> SessionResult<(SessionGroup, usize)> {
        if lifecycle.is_filtered() {
            return Err(SessionError::InvalidOperation(
                "Session lifecycle filtering is unsupported by this view store".into(),
            ));
        }
        self.list_group_sessions(cwd, cursor, limit, session_scope)
            .await
    }

    /// Lifecycle-aware variant of [`ViewStore::list_session_items`].
    async fn list_session_items_with_lifecycle(
        &self,
        cwd: Option<String>,
        cursor: Option<String>,
        limit: usize,
        session_scope: SessionScope,
        lifecycle: SessionLifecycleFilter,
    ) -> SessionResult<(Vec<SessionListItem>, Option<String>, usize)> {
        if lifecycle.is_filtered() {
            return Err(SessionError::InvalidOperation(
                "Session lifecycle filtering is unsupported by this view store".into(),
            ));
        }
        self.list_session_items(cwd, cursor, limit, session_scope)
            .await
    }

    /// ACP flat-list variant that composes an optional persisted pin filter in storage.
    async fn list_session_items_with_pins(
        &self,
        cwd: Option<String>,
        cursor: Option<String>,
        limit: usize,
        session_scope: SessionScope,
        lifecycle: SessionLifecycleFilter,
        pinned: Option<bool>,
    ) -> SessionResult<(Vec<SessionListItem>, Option<String>, usize)> {
        if pinned.is_some() {
            return Err(SessionError::InvalidOperation(
                "Session pin filtering is unsupported by this view store".into(),
            ));
        }
        self.list_session_items_with_lifecycle(cwd, cursor, limit, session_scope, lifecycle)
            .await
    }

    /// Lifecycle-aware variant of [`ViewStore::search_sessions`].
    async fn search_sessions_with_lifecycle(
        &self,
        query: String,
        cursor: Option<String>,
        limit: usize,
        session_scope: SessionScope,
        lifecycle: SessionLifecycleFilter,
    ) -> SessionResult<(Vec<SessionGroup>, Option<String>, usize)> {
        if lifecycle.is_filtered() {
            return Err(SessionError::InvalidOperation(
                "Session lifecycle filtering is unsupported by this view store".into(),
            ));
        }
        self.search_sessions(query, cursor, limit, session_scope)
            .await
    }

    /// Lifecycle-aware variant of [`ViewStore::list_session_children`].
    async fn list_session_children_with_lifecycle(
        &self,
        parent_session_id: String,
        cursor: Option<String>,
        limit: usize,
        lifecycle: SessionLifecycleFilter,
    ) -> SessionResult<(SessionGroup, usize)> {
        if lifecycle.is_filtered() {
            return Err(SessionError::InvalidOperation(
                "Session lifecycle filtering is unsupported by this view store".into(),
            ));
        }
        self.list_session_children(parent_session_id, cursor, limit)
            .await
    }
}
