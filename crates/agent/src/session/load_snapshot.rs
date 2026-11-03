use crate::acp::protocol::ContentBlock;
use crate::agent::LocalAgentHandle;
use crate::events::AgentEvent;
use crate::model::MessagePart;
use crate::session::error::{SessionError, SessionResult};
use crate::session::history_window::{
    HistoryInfo, LoadWindow, SessionHistorySummary, TruncatedEntry, read_window, summarize_history,
};
use crate::session::projection::{AuditView, ViewStore};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use time::OffsetDateTime;
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StreamCursor {
    #[typeshare(serialized_as = "number")]
    pub local_seq: i64,
    #[typeshare(serialized_as = "Record<string, number>")]
    pub remote_seq_by_source: HashMap<String, i64>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserPromptRecord {
    pub message_id: String,
    /// Zero-based position in persisted message history; this is not an event sequence.
    #[typeshare(serialized_as = "number")]
    pub message_order: u64,
    #[typeshare(serialized_as = "number")]
    pub timestamp: i64,
    #[typeshare(serialized_as = "any")]
    pub blocks: Vec<ContentBlock>,
}

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
    pub accounting: crate::accounting::AccountingSnapshot,
    #[serde(rename = "delegationUpdates")]
    pub delegation_updates:
        Vec<crate::control::delegation_notifications::DelegationUpdateNotification>,
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

/// Fold one event into a stream cursor.
pub(crate) fn observe_cursor(cursor: &mut StreamCursor, event: &AgentEvent) {
    match event.origin {
        crate::events::EventOrigin::Local => {
            cursor.local_seq = cursor.local_seq.max(event.seq);
        }
        crate::events::EventOrigin::Remote => {
            if let Some(source) = event.source_node.as_ref() {
                cursor
                    .remote_seq_by_source
                    .entry(source.clone())
                    .and_modify(|seq| *seq = (*seq).max(event.seq))
                    .or_insert(event.seq);
            }
        }
        crate::events::EventOrigin::Unknown(_) => {
            cursor.local_seq = cursor.local_seq.max(event.seq);
        }
    }
}

pub fn cursor_from_events(events: &[AgentEvent]) -> StreamCursor {
    let mut cursor = StreamCursor::default();
    for event in events {
        observe_cursor(&mut cursor, event);
    }
    cursor
}

fn user_prompt_records(messages: &[crate::model::AgentMessage]) -> Vec<UserPromptRecord> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(message_order, message)| {
            let blocks = message.parts.iter().find_map(|part| match part {
                MessagePart::Prompt { blocks } | MessagePart::Steering { blocks, .. } => {
                    Some(blocks.clone())
                }
                _ => None,
            })?;
            Some(UserPromptRecord {
                message_id: message.id.clone(),
                message_order: message_order as u64,
                timestamp: message.created_at,
                blocks,
            })
        })
        .collect()
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
    let accounting = crate::accounting::AccountingSnapshot::from_events(
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
    // `get_history` reports `SessionNotFound`. Treat that as an empty history
    // (no user prompt records) instead of failing the whole snapshot.
    let messages = match agent
        .config
        .provider
        .history_store()
        .get_history(session_id)
        .await
    {
        Ok(messages) => messages,
        Err(SessionError::SessionNotFound(_)) if journal_backed => {
            tracing::debug!(
                session_id,
                "journal-backed session missing local history rows; using empty history for snapshot"
            );
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let user_prompts = Some(user_prompt_records(&messages));
    Ok(SessionLoadSnapshot {
        runtime_status,
        audit,
        cursor,
        accounting,
        delegation_updates,
        user_prompts,
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
    let messages = match history_store.get_history(session_id).await {
        Ok(messages) => messages,
        Err(SessionError::SessionNotFound(_)) if journal_backed => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut prompts = user_prompt_records(&messages);
    if !read.history.complete
        && let Some(first) = audit.events.first()
    {
        // Only prompts at or after the window's first event are returned;
        // older ones arrive with their events.
        let from = first.timestamp;
        prompts.retain(|record| record.timestamp >= from);
    }
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

    use querymt::{LLMParams, chat::ChatRole};

    use super::{SessionLoadSnapshot, load_session_snapshot, user_prompt_records};
    use crate::acp::protocol::{ContentBlock, ImageContent, TextContent};
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::model::{AgentMessage, MessagePart};
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

    #[test]
    fn user_prompt_projection_preserves_message_identity_order_and_blocks() {
        let messages = vec![
            AgentMessage {
                id: "assistant".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Text {
                    content: "response".to_string(),
                }],
                created_at: 10,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
            AgentMessage {
                id: "user-1".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::User,
                parts: vec![MessagePart::Prompt {
                    blocks: vec![
                        ContentBlock::Text(TextContent::new("look")),
                        ContentBlock::Image(ImageContent::new("AQID", "image/png")),
                    ],
                }],
                created_at: 11,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        ];

        let records = user_prompt_records(&messages);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message_id, "user-1");
        // messageOrder is the zero-based persisted history position, not the
        // PromptReceived event's stream sequence.
        assert_eq!(records[0].message_order, 1);
        assert_eq!(records[0].timestamp, 11);
        assert!(matches!(records[0].blocks[1], ContentBlock::Image(_)));
        let value = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(value["messageId"], "user-1");
        assert_eq!(value["messageOrder"], 1);
        assert_eq!(value["blocks"][1]["type"], "image");
    }

    #[test]
    fn user_prompt_projection_includes_steering_parts() {
        let messages = vec![
            AgentMessage {
                id: "assistant".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::Assistant,
                parts: vec![MessagePart::Text {
                    content: "response".to_string(),
                }],
                created_at: 10,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
            AgentMessage {
                id: "steer-1".to_string(),
                session_id: "s1".to_string(),
                role: ChatRole::User,
                parts: vec![MessagePart::Steering {
                    run_id: "run-1".to_string(),
                    client_input_id: Some("cid-1".to_string()),
                    blocks: vec![
                        ContentBlock::Text(TextContent::new("steer")),
                        ContentBlock::Image(ImageContent::new("AQID", "image/png")),
                    ],
                }],
                created_at: 12,
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            },
        ];

        let records = user_prompt_records(&messages);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message_id, "steer-1");
        // messageOrder counts all persisted messages: the text-only assistant
        // message is skipped as a projection source but still occupies index 0.
        assert_eq!(records[0].message_order, 1);
        assert_eq!(records[0].timestamp, 12);
        assert!(matches!(records[0].blocks[0], ContentBlock::Text(_)));
        assert!(matches!(records[0].blocks[1], ContentBlock::Image(_)));
        let value = serde_json::to_value(&records[0]).unwrap();
        assert_eq!(value["messageId"], "steer-1");
        assert_eq!(value["messageOrder"], 1);
        assert_eq!(value["timestamp"], 12);
        assert_eq!(value["blocks"][1]["type"], "image");
    }

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
            .returning(|_| Err(SessionError::DatabaseError("history unavailable".into())))
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
        use crate::events::AgentEventKind;
        use crate::session::run_reconcile::tests::{fixture, seed, started};

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
                crate::accounting::AccountingSnapshot::default()
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
            Err(SessionError::DatabaseError("no audit projection".into()))
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

        async fn get_atif(
            &self,
            _session_id: &str,
            _options: &crate::export::AtifExportOptions,
        ) -> SessionResult<crate::export::ATIF> {
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
        use crate::events::{AgentEventKind, EventOrigin};
        use crate::session::projection::{EventJournal, NewDurableEvent};

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
}
