use async_trait::async_trait;
use time::OffsetDateTime;

use crate::events::AgentEvent;
use crate::session::domain::TaskStatus;
use crate::session::error::SessionResult;
use crate::session::projection::{
    AuditView, DefaultRedactor, EventJournal, FilterExpr, PredicateOp, RecentModelEntry,
    RecentModelsView, RedactedArtifact, RedactedProgress, RedactedTask, RedactedView,
    RedactionPolicy, Redactor, SessionGroup, SessionListFilter, SessionListItem, SessionListView,
    SummaryView, ViewStore,
};
use crate::session::repo_artifact::SqliteArtifactRepository;
use crate::session::repo_decision::SqliteDecisionRepository;
use crate::session::repo_delegation::SqliteDelegationRepository;
use crate::session::repo_intent::SqliteIntentRepository;
use crate::session::repo_progress::SqliteProgressRepository;
use crate::session::repo_session::SqliteSessionRepository;
use crate::session::repo_task::SqliteTaskRepository;
use crate::session::repository::{
    ArtifactRepository, DecisionRepository, DelegationRepository, IntentRepository,
    ProgressRepository, SessionRepository, TaskRepository,
};
use crate::session::store::Session;

use super::SqliteStorage;

#[async_trait]
impl ViewStore for SqliteStorage {
    async fn get_audit_view(
        &self,
        session_id: &str,
        include_children: bool,
    ) -> SessionResult<AuditView> {
        let mut events: Vec<AgentEvent> = self
            .load_session_stream(session_id, None, None)
            .await?
            .into_iter()
            .map(AgentEvent::from)
            .collect();

        // Include child session events (delegations) if requested
        if include_children {
            let session_repo = SqliteSessionRepository::new(self.conn.clone());
            let child_session_ids = session_repo.list_child_sessions(session_id).await?;
            for child_id in &child_session_ids {
                let child_events: Vec<AgentEvent> = self
                    .load_session_stream(child_id, None, None)
                    .await?
                    .into_iter()
                    .map(AgentEvent::from)
                    .collect();
                events.extend(child_events);
            }
        }

        // Sort by sequence number for correct chronological order
        events.sort_by_key(|e| e.seq);

        let task_repo = SqliteTaskRepository::new(self.conn.clone());
        let intent_repo = SqliteIntentRepository::new(self.conn.clone());
        let decision_repo = SqliteDecisionRepository::new(self.conn.clone());
        let progress_repo = SqliteProgressRepository::new(self.conn.clone());
        let artifact_repo = SqliteArtifactRepository::new(self.conn.clone());
        let delegation_repo = SqliteDelegationRepository::new(self.conn.clone());

        let tasks = task_repo.list_tasks(session_id).await?;
        let intent_snapshots = intent_repo.list_intent_snapshots(session_id).await?;
        let decisions = decision_repo.list_decisions(session_id, None).await?;
        let progress_entries = progress_repo
            .list_progress_entries(session_id, None)
            .await?;
        let artifacts = artifact_repo.list_artifacts(session_id, None).await?;
        let delegations = delegation_repo.list_delegations(session_id).await?;

        Ok(AuditView {
            session_id: session_id.to_string(),
            events,
            tasks,
            intent_snapshots,
            decisions,
            progress_entries,
            artifacts,
            delegations,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_redacted_view(
        &self,
        session_id: &str,
        policy: RedactionPolicy,
    ) -> SessionResult<RedactedView> {
        let redactor = DefaultRedactor;

        let intent_repo = SqliteIntentRepository::new(self.conn.clone());
        let task_repo = SqliteTaskRepository::new(self.conn.clone());
        let progress_repo = SqliteProgressRepository::new(self.conn.clone());
        let artifact_repo = SqliteArtifactRepository::new(self.conn.clone());

        // Get current intent
        let intent_snapshots = intent_repo.list_intent_snapshots(session_id).await?;
        let current_intent = intent_snapshots
            .last()
            .map(|s| redactor.redact(&s.summary, policy));

        // Get active task
        let tasks = task_repo.list_tasks(session_id).await?;
        let active_task = tasks
            .iter()
            .find(|t| matches!(t.status, TaskStatus::Active))
            .map(|t| RedactedTask {
                id: t.public_id.clone(),
                status: format!("{:?}", t.status),
                expected_deliverable: t
                    .expected_deliverable
                    .as_ref()
                    .map(|d| redactor.redact(d, policy)),
            });

        // Get recent progress (last 10 entries)
        let all_progress = progress_repo
            .list_progress_entries(session_id, None)
            .await?;
        let recent_progress: Vec<RedactedProgress> = all_progress
            .iter()
            .rev()
            .take(10)
            .map(|p| RedactedProgress {
                kind: format!("{:?}", p.kind),
                summary: redactor.redact(&p.content, policy),
                created_at: p.created_at,
            })
            .collect();

        // Get artifacts
        let all_artifacts = artifact_repo.list_artifacts(session_id, None).await?;
        let artifacts: Vec<RedactedArtifact> = all_artifacts
            .iter()
            .map(|a| RedactedArtifact {
                kind: a.kind.clone(),
                summary: a.summary.as_ref().map(|s| redactor.redact(s, policy)),
                created_at: a.created_at,
            })
            .collect();

        Ok(RedactedView {
            session_id: session_id.to_string(),
            current_intent,
            active_task,
            recent_progress,
            artifacts,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_summary_view(&self, session_id: &str) -> SessionResult<SummaryView> {
        let intent_repo = SqliteIntentRepository::new(self.conn.clone());
        let task_repo = SqliteTaskRepository::new(self.conn.clone());
        let decision_repo = SqliteDecisionRepository::new(self.conn.clone());
        let progress_repo = SqliteProgressRepository::new(self.conn.clone());
        let artifact_repo = SqliteArtifactRepository::new(self.conn.clone());

        // Get current intent
        let intent_snapshots = intent_repo.list_intent_snapshots(session_id).await?;
        let current_intent = intent_snapshots.last().map(|s| s.summary.clone());

        // Get active task status
        let tasks = task_repo.list_tasks(session_id).await?;
        let active_task_status = tasks
            .iter()
            .find(|t| matches!(t.status, TaskStatus::Active))
            .map(|t| format!("{:?}", t.status));

        // Count entities
        let decisions = decision_repo.list_decisions(session_id, None).await?;
        let progress_entries = progress_repo
            .list_progress_entries(session_id, None)
            .await?;
        let artifacts = artifact_repo.list_artifacts(session_id, None).await?;

        // Get last activity
        let last_activity = progress_entries.last().map(|p| {
            format!(
                "{:?}: {}",
                p.kind,
                p.content.chars().take(50).collect::<String>()
            )
        });

        Ok(SummaryView {
            session_id: session_id.to_string(),
            current_intent,
            active_task_status,
            progress_count: progress_entries.len(),
            artifact_count: artifacts.len(),
            decision_count: decisions.len(),
            last_activity,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    #[tracing::instrument(
        name = "session.get_session_list_view",
        skip(self, filter),
        fields(
            session_count = tracing::field::Empty,
            filtered_out_count = tracing::field::Empty,
            total_count = tracing::field::Empty,
            group_count = tracing::field::Empty,
            title_lookup_count = tracing::field::Empty,
            total_ms = tracing::field::Empty,
            list_sessions_ms = tracing::field::Empty,
            filter_ms = tracing::field::Empty,
            title_lookup_ms = tracing::field::Empty,
            hierarchy_build_ms = tracing::field::Empty,
            group_build_ms = tracing::field::Empty
        )
    )]
    async fn get_session_list_view(
        &self,
        filter: Option<SessionListFilter>,
    ) -> SessionResult<SessionListView> {
        use std::collections::HashMap;
        use std::time::Instant;

        let started = Instant::now();

        // ---------------------------------------------------------------------------
        // Single query: fetch sessions with their initial intent title, recurring
        // flag, and has_children flag — replacing the old N+1 per-session lookups.
        // ---------------------------------------------------------------------------
        let list_sessions_started = Instant::now();

        struct RawRow {
            id: i64,
            public_id: String,
            name: Option<String>,
            cwd: Option<String>,
            created_at: Option<String>,
            updated_at: Option<String>,
            parent_session_id_internal: Option<i64>,
            fork_origin: Option<String>,
            session_kind: Option<String>,
            initial_intent: Option<String>,
            is_recurring: bool,
            has_children: bool,
        }

        // Extract SQL-level limit from filter before moving into the closure.
        // A limit without a filter expression can be pushed straight into SQL,
        // avoiding deserializing rows we'll discard anyway.
        let sql_limit: Option<usize> = filter
            .as_ref()
            .and_then(|f| if f.filter.is_none() { f.limit } else { None });

        let raw_rows: Vec<RawRow> = self
            .run_blocking(move |conn| {
                // Build query with optional LIMIT pushed into SQL when there is
                // no predicate filter (predicates still require in-memory eval).
                let sql = if let Some(limit) = sql_limit {
                    format!(
                        r#"
                        SELECT
                            s.id,
                            s.public_id,
                            s.name,
                            s.cwd,
                            s.created_at,
                            s.updated_at,
                            s.parent_session_id,
                            s.fork_origin,
                            s.session_kind,
                            i.summary                                    AS initial_intent,
                            EXISTS(
                                SELECT 1 FROM tasks t
                                WHERE t.session_id = s.id AND t.kind = 'recurring'
                            )                                            AS is_recurring,
                            EXISTS(
                                SELECT 1 FROM sessions c
                                WHERE c.parent_session_id = s.id
                            )                                            AS has_children
                        FROM sessions s
                        LEFT JOIN intent_snapshots i
                            ON i.id = (
                                SELECT MIN(id) FROM intent_snapshots
                                WHERE session_id = s.id
                            )
                        ORDER BY s.updated_at DESC
                        LIMIT {limit}
                        "#
                    )
                } else {
                    r#"
                    SELECT
                        s.id,
                        s.public_id,
                        s.name,
                        s.cwd,
                        s.created_at,
                        s.updated_at,
                        s.parent_session_id,
                        s.fork_origin,
                        s.session_kind,
                        i.summary                                    AS initial_intent,
                        EXISTS(
                            SELECT 1 FROM tasks t
                            WHERE t.session_id = s.id AND t.kind = 'recurring'
                        )                                            AS is_recurring,
                        EXISTS(
                            SELECT 1 FROM sessions c
                            WHERE c.parent_session_id = s.id
                        )                                            AS has_children
                    FROM sessions s
                    LEFT JOIN intent_snapshots i
                        ON i.id = (
                            SELECT MIN(id) FROM intent_snapshots
                            WHERE session_id = s.id
                        )
                    ORDER BY s.updated_at DESC
                    "#
                    .to_string()
                };

                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], |row| {
                    Ok(RawRow {
                        id: row.get(0)?,
                        public_id: row.get(1)?,
                        name: row.get(2)?,
                        cwd: row.get(3)?,
                        created_at: row.get(4)?,
                        updated_at: row.get(5)?,
                        parent_session_id_internal: row.get(6)?,
                        fork_origin: row.get(7)?,
                        session_kind: row.get(8)?,
                        initial_intent: row.get(9)?,
                        is_recurring: row.get::<_, i64>(10)? != 0,
                        has_children: row.get::<_, i64>(11)? != 0,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await?;

        let list_sessions_ms = list_sessions_started.elapsed().as_millis() as u64;
        let session_count_before_filter = raw_rows.len();

        // Build id→public_id map for parent resolution before filtering.
        let id_to_public_id: HashMap<i64, String> = raw_rows
            .iter()
            .map(|r| (r.id, r.public_id.clone()))
            .collect();

        let filter_started = Instant::now();

        // Apply in-memory filter/limit (filter_expr needs the full Session struct;
        // convert only the subset we need to evaluate the predicate).
        let mut raw_rows = raw_rows;
        if let Some(filter_spec) = filter {
            if let Some(filter_expr) = filter_spec.filter {
                // Build minimal Session values for predicate evaluation.
                // evaluate_session_filter only uses fields available in RawRow.
                raw_rows.retain(|r| {
                    // evaluate_session_filter only reads: public_id, name, cwd,
                    // created_at, updated_at — so only those fields need values.
                    let session = Session {
                        id: r.id,
                        public_id: r.public_id.clone(),
                        name: r.name.clone(),
                        cwd: r.cwd.as_deref().map(std::path::PathBuf::from),
                        created_at: r.created_at.as_deref().and_then(|s| {
                            time::OffsetDateTime::parse(
                                s,
                                &time::format_description::well_known::Rfc3339,
                            )
                            .ok()
                        }),
                        updated_at: r.updated_at.as_deref().and_then(|s| {
                            time::OffsetDateTime::parse(
                                s,
                                &time::format_description::well_known::Rfc3339,
                            )
                            .ok()
                        }),
                        current_intent_snapshot_id: None,
                        active_task_id: None,
                        llm_config_id: None,
                        parent_session_id: r.parent_session_id_internal,
                        fork_origin: r.fork_origin.as_deref().and_then(|s| s.parse().ok()),
                        session_kind: r.session_kind.clone(),
                        fork_point_type: None,
                        fork_point_ref: None,
                        fork_instructions: None,
                    };
                    evaluate_session_filter(&session, &filter_expr)
                });
            }
            if let Some(limit) = filter_spec.limit {
                raw_rows.truncate(limit);
            }
        }

        let filter_ms = filter_started.elapsed().as_millis() as u64;
        let total_count = raw_rows.len();

        // Build session list items
        let title_lookup_started = Instant::now();
        let mut items = Vec::with_capacity(raw_rows.len());

        for row in raw_rows {
            let title = row.initial_intent.map(|summary| {
                if summary.len() > 80 {
                    format!("{}...", &summary[..77])
                } else {
                    summary
                }
            });

            let parent_session_id = row
                .parent_session_id_internal
                .and_then(|pid| id_to_public_id.get(&pid).cloned());

            let session_kind = if row.is_recurring {
                Some("recurring".to_string())
            } else {
                row.session_kind
            };

            items.push(SessionListItem {
                session_id: row.public_id,
                name: row.name,
                cwd: row.cwd,
                title,
                created_at: row.created_at.as_deref().and_then(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .ok()
                }),
                updated_at: row.updated_at.as_deref().and_then(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .ok()
                }),
                parent_session_id,
                fork_origin: row.fork_origin,
                session_kind,
                has_children: row.has_children,
            });
        }
        let title_lookup_ms = title_lookup_started.elapsed().as_millis() as u64;

        // Build a parent-child map to organize sessions hierarchically
        let hierarchy_started = Instant::now();
        let mut parent_children_map: HashMap<String, Vec<SessionListItem>> = HashMap::new();
        let mut root_sessions: Vec<SessionListItem> = Vec::new();

        for item in items {
            if let Some(ref parent_id) = item.parent_session_id {
                parent_children_map
                    .entry(parent_id.clone())
                    .or_default()
                    .push(item);
            } else {
                root_sessions.push(item);
            }
        }

        // Recursively attach children to their parents
        fn attach_children(
            session: &mut SessionListItem,
            children_map: &HashMap<String, Vec<SessionListItem>>,
        ) -> Vec<SessionListItem> {
            let mut all_sessions = vec![session.clone()];

            if let Some(children) = children_map.get(&session.session_id) {
                for mut child in children.clone() {
                    let child_descendants = attach_children(&mut child, children_map);
                    all_sessions.extend(child_descendants);
                }
            }

            all_sessions
        }

        // Flatten hierarchy while maintaining parent-child order
        // Filter out delegated child sessions to prevent empty groups
        let mut flat_items = Vec::new();
        for mut root in root_sessions {
            let sessions_with_descendants = attach_children(&mut root, &parent_children_map);
            for session in sessions_with_descendants {
                // Only include parent sessions or non-delegated children
                let is_delegated_child = session.parent_session_id.is_some()
                    && session.fork_origin.as_deref() == Some("delegation");
                if !is_delegated_child {
                    flat_items.push(session);
                }
            }
        }

        let hierarchy_build_ms = hierarchy_started.elapsed().as_millis() as u64;

        // Group by CWD
        let group_build_started = Instant::now();
        let mut groups_map: HashMap<Option<String>, Vec<SessionListItem>> = HashMap::new();
        for item in flat_items {
            groups_map.entry(item.cwd.clone()).or_default().push(item);
        }

        // Convert to SessionGroup vec and sort
        let mut groups: Vec<SessionGroup> = groups_map
            .into_iter()
            .map(|(cwd, sessions)| {
                let latest_activity = sessions.iter().filter_map(|s| s.updated_at).max();
                SessionGroup {
                    cwd,
                    sessions,
                    latest_activity,
                }
            })
            .collect();

        // Sort groups: No-CWD first, then by latest_activity desc
        groups.sort_by(|a, b| {
            match (&a.cwd, &b.cwd) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(_), Some(_)) => {
                    // Both have CWD, sort by latest activity (most recent first)
                    b.latest_activity.cmp(&a.latest_activity)
                }
            }
        });

        let group_count = groups.len();
        let group_build_ms = group_build_started.elapsed().as_millis() as u64;
        let total_ms = started.elapsed().as_millis() as u64;
        let span = tracing::Span::current();
        span.record("session_count", session_count_before_filter);
        span.record(
            "filtered_out_count",
            session_count_before_filter.saturating_sub(total_count),
        );
        span.record("total_count", total_count);
        span.record("group_count", group_count);
        span.record("title_lookup_count", total_count);
        span.record("total_ms", total_ms);
        span.record("list_sessions_ms", list_sessions_ms);
        span.record("filter_ms", filter_ms);
        span.record("title_lookup_ms", title_lookup_ms);
        span.record("hierarchy_build_ms", hierarchy_build_ms);
        span.record("group_build_ms", group_build_ms);

        Ok(SessionListView {
            groups,
            total_count,
            generated_at: OffsetDateTime::now_utc(),
        })
    }

    async fn get_atif(
        &self,
        session_id: &str,
        options: &crate::export::AtifExportOptions,
    ) -> SessionResult<crate::export::ATIF> {
        use crate::export::ATIFBuilder;

        // Get the full audit view which contains all events and domain data
        // Include child sessions for complete trajectory export
        let audit_view = self.get_audit_view(session_id, true).await?;

        // Build the ATIF trajectory from the audit view
        // Tool definitions will be extracted from ToolsAvailable events
        let builder = ATIFBuilder::from_audit_view(&audit_view, options);
        let trajectory = builder.build();

        Ok(trajectory)
    }

    async fn get_recent_models_view(
        &self,
        limit_per_workspace: usize,
    ) -> SessionResult<RecentModelsView> {
        use std::collections::HashMap;

        let conn_arc = self.conn.clone();

        let results = tokio::task::spawn_blocking(
            move || -> Result<Vec<(Option<String>, String, String, i64, u32)>, rusqlite::Error> {
                let conn = conn_arc.lock().unwrap();

                // Query all ProviderChanged events with workspace info.
                // Uses event_journal (the legacy `events` table was dropped
                // by migration 0002).
                // Note: payload_json uses adjacently tagged serde format
                // (tag="type", content="data"), so variant payload is under $.data.*
                let mut stmt = conn.prepare(
                    r#"
                SELECT
                    s.cwd,
                    json_extract(e.payload_json, '$.data.provider') as provider,
                    json_extract(e.payload_json, '$.data.model') as model,
                    MAX(e.timestamp) as last_used_ts,
                    COUNT(*) as use_count
                FROM event_journal e
                JOIN sessions s ON s.public_id = e.session_id
                WHERE e.kind = 'provider_changed'
                  AND provider IS NOT NULL
                  AND model IS NOT NULL
                GROUP BY s.cwd, provider, model
                ORDER BY last_used_ts DESC
                "#,
                )?;

                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, u32>(4)?,
                    ))
                })?;

                let mut results = Vec::new();
                for row in rows {
                    results.push(row?);
                }
                Ok(results)
            },
        )
        .await
        .map_err(|e| {
            crate::session::error::SessionError::Other(format!("Task execution failed: {}", e))
        })?
        .map_err(crate::session::error::SessionError::from)?;

        // Group by workspace and limit per workspace
        let mut by_workspace: HashMap<Option<String>, Vec<RecentModelEntry>> = HashMap::new();

        for (cwd, provider, model, last_used_ts, use_count) in results {
            let entry = RecentModelEntry {
                provider,
                model,
                last_used: OffsetDateTime::from_unix_timestamp(last_used_ts / 1000)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                use_count,
            };

            let workspace_entries = by_workspace.entry(cwd).or_default();
            if workspace_entries.len() < limit_per_workspace {
                workspace_entries.push(entry);
            }
        }

        Ok(RecentModelsView {
            by_workspace,
            generated_at: OffsetDateTime::now_utc(),
        })
    }
}

// ============================================================================
// Session filter evaluation (used by get_session_list_view)
// ============================================================================

/// Evaluate a filter expression against a session
fn evaluate_session_filter(session: &Session, expr: &FilterExpr) -> bool {
    match expr {
        FilterExpr::Predicate(pred) => evaluate_predicate(session, pred),
        FilterExpr::And(exprs) => exprs.iter().all(|e| evaluate_session_filter(session, e)),
        FilterExpr::Or(exprs) => exprs.iter().any(|e| evaluate_session_filter(session, e)),
        FilterExpr::Not(expr) => !evaluate_session_filter(session, expr),
    }
}

/// Evaluate a single predicate against a session
fn evaluate_predicate(
    session: &Session,
    pred: &crate::session::projection::FieldPredicate,
) -> bool {
    use serde_json::json;

    let field_value = match pred.field.as_str() {
        "session_id" | "public_id" => Some(json!(session.public_id)),
        "name" => session.name.as_ref().map(|n| json!(n)),
        "cwd" => session.cwd.as_ref().map(|p| json!(p.display().to_string())),
        "created_at" => session.created_at.map(|t| {
            json!(
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            )
        }),
        "updated_at" => session.updated_at.map(|t| {
            json!(
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            )
        }),
        _ => None,
    };

    match &pred.op {
        PredicateOp::IsNull => field_value.is_none(),
        PredicateOp::IsNotNull => field_value.is_some(),
        PredicateOp::Eq(val) => field_value.as_ref() == Some(val),
        PredicateOp::Ne(val) => field_value.as_ref() != Some(val),
        PredicateOp::Gt(val) => {
            // For string timestamps, compare lexicographically
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv > v,
                _ => false,
            }
        }
        PredicateOp::Gte(val) => {
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv >= v,
                _ => false,
            }
        }
        PredicateOp::Lt(val) => {
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv < v,
                _ => false,
            }
        }
        PredicateOp::Lte(val) => {
            match (field_value.as_ref().and_then(|v| v.as_str()), val.as_str()) {
                (Some(fv), Some(v)) => fv <= v,
                _ => false,
            }
        }
        PredicateOp::Contains(s) => {
            if let Some(fv) = field_value.as_ref().and_then(|v| v.as_str()) {
                fv.contains(s.as_str())
            } else {
                false
            }
        }
        PredicateOp::StartsWith(s) => {
            if let Some(fv) = field_value.as_ref().and_then(|v| v.as_str()) {
                fv.starts_with(s.as_str())
            } else {
                false
            }
        }
        PredicateOp::In(vals) => field_value.as_ref().is_some_and(|fv| vals.contains(fv)),
    }
}
