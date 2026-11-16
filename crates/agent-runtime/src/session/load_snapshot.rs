use crate::agent::LocalAgentHandle;
use crate::session::error::{SessionError, SessionResult};
use crate::session::history_window::{
    HistoryInfo, LoadWindow, SessionHistorySummary, TruncatedEntry, read_window, summarize_history,
};
use crate::session::projection::{AuditView, ViewStore};
use querymt_agent_core::events::AgentEvent;
pub use querymt_agent_core::session::prompt_records::UserPromptRecord;
pub(crate) use querymt_agent_core::session::prompt_records::{
    filter_prompt_records, user_prompt_records,
};
pub use querymt_agent_core::stream_cursor::{StreamCursor, observe_cursor};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use time::OffsetDateTime;
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLoadSnapshot {
    #[serde(rename = "runtimeStatus")]
    pub runtime_status: crate::agent::messages::SessionRuntimeStatus,
    pub audit: AuditView,
    pub cursor: StreamCursor,
    /// Authoritative replacement accounting at `cursor`, reduced from the same
    /// audit events by the shared accounting reducer. Present even when
    /// historical replay is skipped or the context limit is unknown.
    #[typeshare(serialized_as = "AccountingSnapshot")]
    pub accounting: querymt_agent_services::accounting::AccountingSnapshot,
    #[serde(rename = "delegationUpdates")]
    pub delegation_updates:
        Vec<querymt_agent_core::delegation_update::DelegationUpdateNotification>,
    #[serde(
        default,
        rename = "userPrompts",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_prompts: Option<Vec<UserPromptRecord>>,
    /// Present only for windowed loads: where the returned events sit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryInfo>,
    /// Present only for windowed loads: whole-session state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SessionHistorySummary>,
    /// Present only for windowed loads that shortened tool content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<Vec<TruncatedEntry>>,
}

pub fn cursor_from_events(events: &[AgentEvent]) -> StreamCursor {
    let mut cursor = StreamCursor::default();
    for event in events {
        observe_cursor(&mut cursor, event);
    }
    cursor
}

/// Message ids of the prompts (`PromptReceived` with an id) among `events`,
/// in event order, without duplicates.
pub(crate) fn prompt_message_ids(events: &[AgentEvent]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for event in events {
        if let querymt_agent_core::events::AgentEventKind::PromptReceived {
            message_id: Some(id),
            ..
        } = &event.kind
            && !ids.contains(id)
        {
            ids.push(id.clone());
        }
    }
    ids
}

/// Add records whose message id is not present yet, keeping message order.
pub(crate) fn merge_prompt_records(into: &mut Vec<UserPromptRecord>, more: Vec<UserPromptRecord>) {
    let mut added = false;
    for record in more {
        if !into
            .iter()
            .any(|known| known.message_id == record.message_id)
        {
            into.push(record);
            added = true;
        }
    }
    if added {
        into.sort_by_key(|record| record.message_order);
    }
}

pub async fn load_session_snapshot(
    agent: &LocalAgentHandle,
    view_store: Arc<dyn ViewStore>,
    session_id: &str,
) -> SessionResult<SessionLoadSnapshot> {
    load_session_snapshot_windowed(agent, view_store, session_id, None).await
}

/// Build a load snapshot. Without a `window` this is exactly the full,
/// untruncated snapshot; with one, only the most recent turns are returned
/// together with a whole-session summary (see `history_window`).
pub async fn load_session_snapshot_windowed(
    agent: &LocalAgentHandle,
    view_store: Arc<dyn ViewStore>,
    session_id: &str,
    window: Option<LoadWindow>,
) -> SessionResult<SessionLoadSnapshot> {
    if let Some(window) = window {
        return windowed_snapshot(agent, view_store, session_id, window.clamped()).await;
    }
    let bookmark = agent
        .config
        .provider
        .history_store()
        .get_remote_session_bookmark(session_id)
        .await?;
    let journal_backed = bookmark.is_some();
    let mut positions = None;
    let audit = if let Some(bookmark) = bookmark {
        let (audit, history_positions) =
            journal_backed_audit_view(agent, session_id, &bookmark.node_id).await?;
        positions = Some(history_positions);
        audit
    } else {
        // Close a dead process's run before the audit view is read, so the
        // served snapshot already contains the closing event.
        crate::session::run_reconcile::reconcile_orphaned_run(&agent.config, session_id).await;
        view_store.get_audit_view(session_id, false).await?
    };
    // The cursor keeps its meaning (local `stream_seq`), so it is computed
    // before remote events switch to their history position.
    let cursor = cursor_from_events(&audit.events);
    let mut audit = audit;
    if let Some(positions) = positions {
        for (event, position) in audit.events.iter_mut().zip(positions) {
            event.seq = position;
        }
    }
    let accounting = querymt_agent_services::accounting::AccountingSnapshot::from_events(
        session_id,
        audit.events.iter().map(|event| &event.kind),
    );
    let mut delegation_updates =
        crate::control::delegation_notifications::delegation_updates_from_events(&audit.events);
    let runtime_status = agent
        .observe_session_runtimes(&[session_id.to_string()])
        .await
        .remove(session_id)
        .unwrap_or_else(|| {
            crate::agent::messages::SessionRuntimeStatus::unavailable("observation_unavailable")
        });
    for live in &runtime_status.delegation_activity {
        if let Some(previous) = delegation_updates
            .iter_mut()
            .find(|entry| entry.delegation_id == live.delegation_id)
        {
            *previous = live.clone();
        } else {
            delegation_updates.push(live.clone());
        }
    }
    // Journal-backed sessions (remote attached, or merely bookmarked while
    // offline) live on their peer and have no local history rows, so
    // the prompt read reports `SessionNotFound`. Treat that as no user prompt
    // records instead of failing the whole snapshot.
    let prompts = match agent
        .config
        .provider
        .history_store()
        .get_user_prompts(session_id, None)
        .await
    {
        Ok(prompts) => prompts,
        Err(SessionError::SessionNotFound(_)) if journal_backed => {
            tracing::debug!(
                session_id,
                "journal-backed session missing local history rows; using empty history for snapshot"
            );
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    Ok(SessionLoadSnapshot {
        runtime_status,
        audit,
        cursor,
        accounting,
        delegation_updates,
        user_prompts: Some(prompts),
        history: None,
        summary: None,
        truncated: None,
    })
}

async fn windowed_snapshot(
    agent: &LocalAgentHandle,
    view_store: Arc<dyn ViewStore>,
    session_id: &str,
    window: LoadWindow,
) -> SessionResult<SessionLoadSnapshot> {
    let history_store = agent.config.provider.history_store();
    let bookmark = history_store
        .get_remote_session_bookmark(session_id)
        .await?;
    let journal_backed = bookmark.is_some();
    let journal = agent.config.event_sink.journal().clone();
    let source_node = bookmark.as_ref().map(|bookmark| bookmark.node_id.clone());
    let mut audit = match &bookmark {
        Some(_) => AuditView {
            session_id: session_id.to_string(),
            events: Vec::new(),
            tasks: Vec::new(),
            intent_snapshots: Vec::new(),
            decisions: Vec::new(),
            progress_entries: Vec::new(),
            artifacts: Vec::new(),
            delegations: Vec::new(),
            generated_at: OffsetDateTime::now_utc(),
        },
        None => {
            crate::session::run_reconcile::reconcile_orphaned_run(&agent.config, session_id).await;
            view_store.get_audit_view_metadata(session_id).await?
        }
    };
    let epoch = match source_node.as_deref() {
        Some(node) => journal
            .remote_history_epoch(session_id, node)
            .await?
            .unwrap_or(0),
        None => journal.history_epoch(session_id).await?,
    };
    // Runtime observation is independent of the journal reads; a failure
    // there only makes the runtime status unavailable.
    let observe = async {
        agent
            .observe_session_runtimes(&[session_id.to_string()])
            .await
            .remove(session_id)
            .unwrap_or_else(|| {
                crate::agent::messages::SessionRuntimeStatus::unavailable("observation_unavailable")
            })
    };
    let read_journal = async {
        let read = read_window(
            journal.as_ref(),
            session_id,
            source_node.as_deref(),
            None,
            epoch,
            &window,
        )
        .await?;
        let window_start = if read.history.complete {
            None
        } else {
            read.events.first().map(|event| event.seq)
        };
        let pass = summarize_history(
            journal.as_ref(),
            session_id,
            source_node.as_deref(),
            window_start,
        )
        .await?;
        SessionResult::Ok((read, pass))
    };
    let (journal_result, runtime_status) = tokio::join!(read_journal, observe);
    let (read, pass) = journal_result?;
    audit.events = read.events;

    let mut delegation_updates = pass.delegation_updates;
    for live in &runtime_status.delegation_activity {
        if let Some(previous) = delegation_updates
            .iter_mut()
            .find(|entry| entry.delegation_id == live.delegation_id)
        {
            *previous = live.clone();
        } else {
            delegation_updates.push(live.clone());
        }
    }
    // A window that covers the whole session carries every prompt; otherwise
    // only the prompts of the window's own prompt events (older ones arrive
    // with their events).
    let window_prompt_ids = (!read.history.complete).then(|| prompt_message_ids(&audit.events));
    let prompts = match history_store
        .get_user_prompts(session_id, window_prompt_ids.as_deref())
        .await
    {
        Ok(prompts) => prompts,
        Err(SessionError::SessionNotFound(_)) if journal_backed => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok(SessionLoadSnapshot {
        runtime_status,
        audit,
        cursor: pass.cursor,
        accounting: pass.accounting,
        delegation_updates,
        user_prompts: Some(prompts),
        history: Some(read.history),
        summary: Some(pass.summary),
        truncated: (!read.truncated.is_empty()).then_some(read.truncated),
    })
}

/// Build an audit view from the durable local event journal. This is how
/// cached remote history stays readable while the host is offline; an empty
/// journal produces an empty disconnected view rather than an error.
async fn journal_backed_audit_view(
    agent: &LocalAgentHandle,
    session_id: &str,
    node_id: &str,
) -> SessionResult<(AuditView, Vec<i64>)> {
    // Owner order, covered ranges only. Each event's history position is the
    // owner's `source_seq`; the caller stamps it into `AgentEvent.seq` so the
    // desktop's `eventIndex` ordering follows the owner.
    let history = agent
        .config
        .event_sink
        .journal()
        .load_remote_history(session_id, node_id)
        .await?;
    let positions: Vec<i64> = history.iter().map(|(position, _)| *position).collect();
    let events = history
        .into_iter()
        .map(|(_, event)| AgentEvent::from(event))
        .collect();

    let view = AuditView {
        session_id: session_id.to_string(),
        events,
        tasks: Vec::new(),
        intent_snapshots: Vec::new(),
        decisions: Vec::new(),
        progress_entries: Vec::new(),
        artifacts: Vec::new(),
        delegations: Vec::new(),
        generated_at: OffsetDateTime::now_utc(),
    };
    Ok((view, positions))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use querymt::LLMParams;

    use super::{SessionLoadSnapshot, load_session_snapshot};
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::session::backend::StorageBackend;
    use crate::session::error::{SessionError, SessionResult};
    use crate::session::projection::{
        AuditView, RecentModelsView, RedactedView, RedactionPolicy, SessionGroup,
        SessionListFilter, SessionListItem, SessionListMetaStats, SessionListView, SessionScope,
        SummaryView, ViewStore,
    };
    use crate::session::provider::SessionProvider;
    use crate::session::store::{RemoteSessionBookmark, SessionStore};
    use crate::test_utils::{MockSessionStore, empty_plugin_registry};

    #[tokio::test]
    async fn snapshot_history_failure_is_propagated() {
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let session = storage
            .create_session(None, None, None, None)
            .await
            .unwrap();
        let mut store = MockSessionStore::new();
        store
            .expect_get_remote_session_bookmark()
            .returning(|_| Ok(None))
            .times(2);
        let observed_session = session.clone();
        store
            .expect_get_session()
            .returning(move |_| Ok(Some(observed_session.clone())))
            .times(1);
        store
            .expect_get_session_runtime_binding()
            .returning(|_| Ok(None))
            .times(1);
        store
            .expect_get_history()
            .withf({
                let session_id = session.public_id.clone();
                move |actual| actual == session_id
            })
            .returning(|_| Err(SessionError::Other("history unavailable".into())))
            .times(1);
        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let provider = Arc::new(SessionProvider::new(
            Arc::new(plugin_registry),
            Arc::new(store),
            LLMParams::new().provider("mock").model("mock-model"),
        ));
        let config = Arc::new(
            AgentConfigBuilder::from_provider(storage.clone(), provider, storage.event_journal())
                .build(),
        );
        let agent = crate::agent::LocalAgentHandle::from_config(config);

        let error =
            load_session_snapshot(&agent, storage.view_store().unwrap(), &session.public_id)
                .await
                .unwrap_err();
        assert!(error.to_string().contains("history unavailable"));
    }

    #[tokio::test]
    async fn snapshot_reconciles_orphaned_run_unless_lease_is_held() {
        use crate::session::run_reconcile::tests::{fixture, seed, started};
        use querymt_agent_core::events::AgentEventKind;

        let f = fixture().await;
        let agent = crate::agent::LocalAgentHandle::from_config(f.config.clone());
        let held_id = f
            .storage
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;
        let free_id = f
            .storage
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;
        seed(&f.storage, &held_id, started("held-run")).await;
        seed(&f.storage, &free_id, started("orphan")).await;

        let last_event =
            |snapshot: &SessionLoadSnapshot| snapshot.audit.events.last().map(|e| e.kind.clone());

        // A live owner keeps its run open.
        let lease = f.lease_root.try_acquire(&held_id).unwrap().unwrap();
        let held = load_session_snapshot(&agent, f.storage.view_store().unwrap(), &held_id)
            .await
            .unwrap();
        assert!(matches!(
            last_event(&held),
            Some(AgentEventKind::RunStarted { .. })
        ));
        drop(lease);

        // An orphan is closed before the snapshot is served.
        let free = load_session_snapshot(&agent, f.storage.view_store().unwrap(), &free_id)
            .await
            .unwrap();
        assert!(matches!(
            last_event(&free),
            Some(AgentEventKind::RunCompleted { run_id, outcome, .. })
                if run_id == "orphan" && outcome == "interrupted"
        ));
    }

    #[test]
    fn old_snapshot_without_user_prompts_deserializes() {
        let value = serde_json::json!({
            "runtimeStatus": crate::agent::messages::SessionRuntimeStatus::unavailable("snapshot_fixture"),
            "audit": {
                "session_id": "s1",
                "events": [],
                "tasks": [],
                "intent_snapshots": [],
                "decisions": [],
                "progress_entries": [],
                "artifacts": [],
                "delegations": [],
                "generated_at": "1970-01-01T00:00:00Z"
            },
            "cursor": {"local_seq": 0, "remote_seq_by_source": {}},
            "accounting": serde_json::to_value(
                querymt_agent_services::accounting::AccountingSnapshot::default()
            ).unwrap(),
            "delegationUpdates": []
        });
        let snapshot: SessionLoadSnapshot = serde_json::from_value(value).unwrap();
        assert!(snapshot.user_prompts.is_none());
    }

    /// View store whose audit lookup always fails, forcing the journal-backed
    /// fallback path. `load_session_snapshot` only consults `get_audit_view`;
    /// the remaining methods are unreachable stubs.
    struct FailingAuditViewStore;

    #[async_trait::async_trait]
    impl ViewStore for FailingAuditViewStore {
        async fn get_audit_view(
            &self,
            _session_id: &str,
            _include_children: bool,
        ) -> SessionResult<AuditView> {
            Err(SessionError::Other("no audit projection".into()))
        }

        async fn get_redacted_view(
            &self,
            _session_id: &str,
            _policy: RedactionPolicy,
        ) -> SessionResult<RedactedView> {
            unimplemented!()
        }

        async fn get_summary_view(&self, _session_id: &str) -> SessionResult<SummaryView> {
            unimplemented!()
        }

        async fn get_session_list_view(
            &self,
            _filter: Option<SessionListFilter>,
        ) -> SessionResult<SessionListView> {
            unimplemented!()
        }

        async fn browse_session_groups(
            &self,
            _cursor: Option<String>,
            _group_limit: usize,
            _session_limit_per_group: usize,
            _session_scope: SessionScope,
        ) -> SessionResult<(Vec<SessionGroup>, Option<String>, usize)> {
            unimplemented!()
        }

        async fn list_group_sessions(
            &self,
            _cwd: Option<String>,
            _cursor: Option<String>,
            _limit: usize,
            _session_scope: SessionScope,
        ) -> SessionResult<(SessionGroup, usize)> {
            unimplemented!()
        }

        async fn list_session_items(
            &self,
            _cwd: Option<String>,
            _cursor: Option<String>,
            _limit: usize,
            _session_scope: SessionScope,
        ) -> SessionResult<(Vec<SessionListItem>, Option<String>, usize)> {
            unimplemented!()
        }

        async fn get_session_list_meta_stats(
            &self,
            _session_ids: &[String],
        ) -> SessionResult<std::collections::HashMap<String, SessionListMetaStats>> {
            unimplemented!()
        }

        async fn search_sessions(
            &self,
            _query: String,
            _cursor: Option<String>,
            _limit: usize,
            _session_scope: SessionScope,
        ) -> SessionResult<(Vec<SessionGroup>, Option<String>, usize)> {
            unimplemented!()
        }

        async fn list_session_children(
            &self,
            _parent_session_id: String,
            _cursor: Option<String>,
            _limit: usize,
        ) -> SessionResult<(SessionGroup, usize)> {
            unimplemented!()
        }

        async fn get_recent_models_view(
            &self,
            _limit_per_workspace: usize,
        ) -> SessionResult<RecentModelsView> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn bookmarked_unattached_session_tolerates_missing_history() {
        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let session_id = "remote-bookmarked-1".to_string();
        let mut store = MockSessionStore::new();
        store
            .expect_get_history()
            .withf({
                let session_id = session_id.clone();
                move |actual| actual == session_id
            })
            .returning(|_| Err(SessionError::SessionNotFound("no local history".into())))
            .times(1);
        store
            .expect_get_remote_session_bookmark()
            .withf({
                let session_id = session_id.clone();
                move |actual| actual == session_id
            })
            .returning({
                let session_id = session_id.clone();
                move |_| {
                    Ok(Some(RemoteSessionBookmark {
                        session_id: session_id.clone(),
                        node_id: "node-abc".to_string(),
                        peer_label: "peer-abc".to_string(),
                        cwd: None,
                        created_at: 0,
                        title: None,
                        fork_origin: None,
                    }))
                }
            })
            .times(2);
        store.expect_get_session().returning(|_| Ok(None)).times(1);
        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let provider = Arc::new(SessionProvider::new(
            Arc::new(plugin_registry),
            Arc::new(store),
            LLMParams::new().provider("mock").model("mock-model"),
        ));
        let config = Arc::new(
            AgentConfigBuilder::from_provider(storage.clone(), provider, storage.event_journal())
                .build(),
        );
        let agent = crate::agent::LocalAgentHandle::from_config(config);

        // Bookmarked but unattached remote session: the audit projection is
        // missing (view store errors), so the snapshot is journal-backed, and
        // `get_history` reports `SessionNotFound`. The snapshot must still
        // succeed with an empty user-prompt list.
        let snapshot = load_session_snapshot(&agent, Arc::new(FailingAuditViewStore), &session_id)
            .await
            .unwrap();
        assert!(snapshot.audit.events.is_empty());
        assert_eq!(snapshot.user_prompts, Some(Vec::new()));
    }

    /// Tail-first persisted remote history: reads and snapshots come back in
    /// owner order, only covered ranges are visible, and each event's `seq`
    /// is its history position (the owner's `source_seq`).
    #[tokio::test]
    async fn remote_snapshot_is_owner_ordered_covered_only_with_history_positions() {
        use crate::session::projection::NewDurableEvent;
        use querymt_agent_core::events::{AgentEventKind, EventOrigin};

        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap(),
        );
        let session_id = "remote-tail-first".to_string();
        let journal = storage.event_journal();
        // Arrival order: tail (source 8..=10) first, a stray uncovered event
        // (source 5), then the older fill (source 1..=4).
        for source_seq in [8, 9, 10, 5, 6, 7, 1, 2, 3, 4] {
            journal
                .append_durable_from_source(&NewDurableEvent {
                    session_id: session_id.clone(),
                    origin: EventOrigin::Remote,
                    source_node: Some("peer-abc".into()),
                    source_node_id: Some("node-abc".into()),
                    source_seq: Some(source_seq),
                    session_seq: None,
                    kind: AgentEventKind::SessionCreated,
                })
                .await
                .unwrap();
        }
        journal
            .add_remote_coverage(&session_id, "node-abc", 8, 10)
            .await
            .unwrap();
        // Before the fill completes only the tail window is visible, ordered.
        let tail: Vec<i64> = journal
            .load_remote_history(&session_id, "node-abc")
            .await
            .unwrap()
            .into_iter()
            .map(|(position, _)| position)
            .collect();
        assert_eq!(tail, vec![8, 9, 10], "uncovered events are not returned");

        journal
            .add_remote_coverage(&session_id, "node-abc", 1, 4)
            .await
            .unwrap();
        journal
            .add_remote_coverage(&session_id, "node-abc", 5, 7)
            .await
            .unwrap();
        let mut store = MockSessionStore::new();
        store
            .expect_get_history()
            .returning(|_| Err(SessionError::SessionNotFound("no local history".into())));
        store.expect_get_remote_session_bookmark().returning({
            let session_id = session_id.clone();
            move |_| {
                Ok(Some(RemoteSessionBookmark {
                    session_id: session_id.clone(),
                    node_id: "node-abc".to_string(),
                    peer_label: "peer-abc".to_string(),
                    cwd: None,
                    created_at: 0,
                    title: None,
                    fork_origin: None,
                }))
            }
        });
        store.expect_get_session().returning(|_| Ok(None));
        let (plugin_registry, _temp_dir) = empty_plugin_registry().unwrap();
        let provider = Arc::new(SessionProvider::new(
            Arc::new(plugin_registry),
            Arc::new(store),
            LLMParams::new().provider("mock").model("mock-model"),
        ));
        let config = Arc::new(
            AgentConfigBuilder::from_provider(storage.clone(), provider, storage.event_journal())
                .build(),
        );
        let agent = crate::agent::LocalAgentHandle::from_config(config);

        let snapshot = load_session_snapshot(&agent, Arc::new(FailingAuditViewStore), &session_id)
            .await
            .unwrap();
        let seqs: Vec<i64> = snapshot.audit.events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=10).collect::<Vec<_>>(), "owner order, no gaps");
    }

    /// A local session with `runs` runs. Each run journals RunStarted,
    /// PromptReceived (with the prompt's message id) and RunCompleted, and
    /// stores the prompt message `prompt-{i}`. All journal events and messages
    /// share `timestamp` when given.
    async fn session_with_runs(
        f: &crate::session::run_reconcile::tests::Fixture,
        runs: usize,
        timestamp: Option<i64>,
    ) -> String {
        use crate::session::run_reconcile::tests::seed;
        use querymt::chat::ChatRole;
        use querymt_agent_core::events::AgentEventKind;
        use querymt_agent_core::model::{AgentMessage, MessagePart};

        let sid = f
            .storage
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;
        for i in 0..runs {
            let run_id = format!("run-{i}");
            let message_id = format!("prompt-{i}");
            seed(
                &f.storage,
                &sid,
                AgentEventKind::RunStarted {
                    run_id: run_id.clone(),
                    origin: "interactive".into(),
                },
            )
            .await;
            seed(
                &f.storage,
                &sid,
                AgentEventKind::PromptReceived {
                    content: format!("prompt {i}"),
                    message_id: Some(message_id.clone()),
                },
            )
            .await;
            seed(
                &f.storage,
                &sid,
                AgentEventKind::RunCompleted {
                    run_id,
                    outcome: "completed".into(),
                    stop_type: None,
                    reason: None,
                },
            )
            .await;
            f.storage
                .add_message(
                    &sid,
                    AgentMessage {
                        id: message_id,
                        session_id: sid.clone(),
                        role: ChatRole::User,
                        parts: vec![MessagePart::Prompt {
                            blocks: vec![crate::acp::protocol::ContentBlock::Text(
                                crate::acp::protocol::TextContent::new(format!("prompt {i}")),
                            )],
                        }],
                        created_at: timestamp.unwrap_or(1_000 + i as i64),
                        parent_message_id: None,
                        source_provider: None,
                        source_model: None,
                    },
                )
                .await
                .unwrap();
        }
        if let Some(timestamp) = timestamp {
            f.storage
                .conn_for_test()
                .lock()
                .execute(
                    "UPDATE event_journal SET timestamp = ?1 WHERE session_id = ?2",
                    rusqlite::params![timestamp, sid],
                )
                .unwrap();
        }
        sid
    }

    fn prompt_ids(snapshot: &SessionLoadSnapshot) -> Vec<String> {
        snapshot
            .user_prompts
            .as_ref()
            .expect("user prompts present")
            .iter()
            .map(|record| record.message_id.clone())
            .collect()
    }

    async fn windowed(
        f: &crate::session::run_reconcile::tests::Fixture,
        sid: &str,
        max_turns: usize,
    ) -> SessionLoadSnapshot {
        let agent = crate::agent::LocalAgentHandle::from_config(f.config.clone());
        super::load_session_snapshot_windowed(
            &agent,
            f.storage.view_store().unwrap(),
            sid,
            Some(crate::session::history_window::LoadWindow {
                max_turns,
                ..Default::default()
            }),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn full_load_includes_every_prompt() {
        let f = crate::session::run_reconcile::tests::fixture().await;
        let sid = session_with_runs(&f, 30, None).await;
        let agent = crate::agent::LocalAgentHandle::from_config(f.config.clone());
        let snapshot = load_session_snapshot(&agent, f.storage.view_store().unwrap(), &sid)
            .await
            .unwrap();
        let expected: Vec<String> = (0..30).map(|i| format!("prompt-{i}")).collect();
        assert_eq!(prompt_ids(&snapshot), expected);
    }

    #[tokio::test]
    async fn windowed_load_includes_only_window_prompts() {
        let f = crate::session::run_reconcile::tests::fixture().await;
        let sid = session_with_runs(&f, 30, None).await;
        let snapshot = windowed(&f, &sid, 20).await;
        assert!(!snapshot.history.as_ref().unwrap().complete);
        let expected: Vec<String> = (10..30).map(|i| format!("prompt-{i}")).collect();
        assert_eq!(prompt_ids(&snapshot), expected);
    }

    /// A fork copies prompt/assistant events with the fork's own message ids.
    /// A windowed load of an inherited conversation must still return the
    /// structured records of the prompts in its window.
    #[tokio::test]
    async fn windowed_load_of_a_fork_includes_inherited_prompt_records() {
        use crate::session::run_reconcile::tests::seed;
        use querymt::chat::ChatRole;
        use querymt_agent_core::events::AgentEventKind;
        use querymt_agent_core::model::{AgentMessage, MessagePart};
        use querymt_agent_core::session::domain::ForkOrigin;
        use querymt_agent_core::session::store::SessionStore;

        let f = crate::session::run_reconcile::tests::fixture().await;
        let sid = f
            .storage
            .create_session(None, None, None, None)
            .await
            .unwrap()
            .public_id;
        // Assistant events between prompts keep each prompt its own turn.
        for i in 0..30i64 {
            let prompt_id = format!("prompt-{i}");
            let reply_id = format!("reply-{i}");
            seed(
                &f.storage,
                &sid,
                AgentEventKind::PromptReceived {
                    content: format!("prompt {i}"),
                    message_id: Some(prompt_id.clone()),
                },
            )
            .await;
            seed(
                &f.storage,
                &sid,
                AgentEventKind::AssistantMessageStored {
                    content: format!("reply {i}"),
                    thinking: None,
                    reasoning_parts: Vec::new(),
                    message_id: Some(reply_id.clone()),
                },
            )
            .await;
            for (id, role, part) in [
                (
                    prompt_id,
                    ChatRole::User,
                    MessagePart::Prompt {
                        blocks: vec![crate::acp::protocol::ContentBlock::Text(
                            crate::acp::protocol::TextContent::new(format!("prompt {i}")),
                        )],
                    },
                ),
                (
                    reply_id,
                    ChatRole::Assistant,
                    MessagePart::Text {
                        content: format!("reply {i}"),
                    },
                ),
            ] {
                f.storage
                    .add_message(
                        &sid,
                        AgentMessage {
                            id,
                            session_id: sid.clone(),
                            role,
                            parts: vec![part],
                            created_at: 1_000 + i,
                            parent_message_id: None,
                            source_provider: None,
                            source_model: None,
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        let fork = f
            .storage
            .fork_session(&sid, "reply-29", ForkOrigin::User)
            .await
            .unwrap();

        let snapshot = windowed(&f, &fork, 20).await;
        assert!(
            !snapshot.history.as_ref().unwrap().complete,
            "30 inherited turns must not fit a 20-turn window"
        );
        let window_ids = super::prompt_message_ids(&snapshot.audit.events);
        assert_eq!(window_ids.len(), 20);
        assert_eq!(prompt_ids(&snapshot), window_ids);
        for record in snapshot.user_prompts.as_ref().unwrap() {
            assert_eq!(record.blocks.len(), 1, "structured blocks are included");
        }
    }

    #[tokio::test]
    async fn windowed_load_of_a_short_session_includes_every_prompt() {
        let f = crate::session::run_reconcile::tests::fixture().await;
        let sid = session_with_runs(&f, 5, None).await;
        let snapshot = windowed(&f, &sid, 20).await;
        assert!(snapshot.history.as_ref().unwrap().complete);
        let expected: Vec<String> = (0..5).map(|i| format!("prompt-{i}")).collect();
        assert_eq!(prompt_ids(&snapshot), expected);
    }

    #[tokio::test]
    async fn window_first_prompt_shares_a_second_with_an_older_prompt() {
        let f = crate::session::run_reconcile::tests::fixture().await;
        // Every event and prompt is stored in the same second; a one-turn
        // window holds only the last run.
        let sid = session_with_runs(&f, 3, Some(5_000)).await;
        let snapshot = windowed(&f, &sid, 1).await;
        assert!(!snapshot.history.as_ref().unwrap().complete);
        assert_eq!(prompt_ids(&snapshot), vec!["prompt-2".to_string()]);
    }

    #[tokio::test]
    async fn corrupt_tool_result_does_not_fail_a_load() {
        use querymt::chat::ChatRole;
        use querymt_agent_core::model::{AgentMessage, MessagePart};

        let f = crate::session::run_reconcile::tests::fixture().await;
        let sid = session_with_runs(&f, 3, None).await;
        f.storage
            .add_message(
                &sid,
                AgentMessage {
                    id: "tool-1".into(),
                    session_id: sid.clone(),
                    role: ChatRole::User,
                    parts: vec![MessagePart::ToolResult {
                        call_id: "call-1".into(),
                        content: vec![querymt::chat::ToolResultPart::text("output")],
                        is_error: false,
                        tool_name: Some("read".into()),
                        tool_arguments: None,
                        compacted_at: None,
                    }],
                    created_at: 2_000,
                    parent_message_id: None,
                    source_provider: None,
                    source_model: None,
                },
            )
            .await
            .unwrap();
        let changed = f
            .storage
            .conn_for_test()
            .lock()
            .execute(
                "UPDATE message_parts SET content_json = '{not json' WHERE part_type = 'tool_result'",
                [],
            )
            .unwrap();
        assert_eq!(changed, 1);
        assert!(f.storage.get_history(&sid).await.is_err());

        let agent = crate::agent::LocalAgentHandle::from_config(f.config.clone());
        let snapshot = load_session_snapshot(&agent, f.storage.view_store().unwrap(), &sid)
            .await
            .unwrap();
        assert_eq!(prompt_ids(&snapshot).len(), 3);
    }
}
