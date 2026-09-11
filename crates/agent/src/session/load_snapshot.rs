use crate::acp::protocol::ContentBlock;
use crate::agent::LocalAgentHandle;
use crate::events::AgentEvent;
use crate::model::MessagePart;
use crate::session::error::{SessionError, SessionResult};
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
    pub audit: AuditView,
    pub cursor: StreamCursor,
    #[serde(rename = "delegationUpdates")]
    pub delegation_updates:
        Vec<crate::control::delegation_notifications::DelegationUpdateNotification>,
    #[serde(
        default,
        rename = "userPrompts",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_prompts: Option<Vec<UserPromptRecord>>,
}

pub fn cursor_from_events(events: &[AgentEvent]) -> StreamCursor {
    let mut cursor = StreamCursor::default();

    for event in events {
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
    let bookmark = agent
        .config
        .provider
        .history_store()
        .get_remote_session_bookmark(session_id)
        .await?;
    let journal_backed = bookmark.is_some();
    let audit = if let Some(bookmark) = bookmark {
        journal_backed_audit_view(agent, session_id, &bookmark.node_id).await?
    } else {
        view_store.get_audit_view(session_id, false).await?
    };
    let cursor = cursor_from_events(&audit.events);
    let delegation_updates =
        crate::control::delegation_notifications::delegation_updates_from_events(&audit.events);
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
        audit,
        cursor,
        delegation_updates,
        user_prompts,
    })
}

/// Build an audit view from the durable local event journal. This is how
/// cached remote history stays readable while the host is offline; an empty
/// journal produces an empty disconnected view rather than an error.
async fn journal_backed_audit_view(
    agent: &LocalAgentHandle,
    session_id: &str,
    node_id: &str,
) -> SessionResult<AuditView> {
    let events = agent
        .config
        .event_sink
        .journal()
        .load_remote_session_stream(session_id, node_id)
        .await?
        .into_iter()
        .map(AgentEvent::from)
        .collect();

    Ok(AuditView {
        session_id: session_id.to_string(),
        events,
        tasks: Vec::new(),
        intent_snapshots: Vec::new(),
        decisions: Vec::new(),
        progress_entries: Vec::new(),
        artifacts: Vec::new(),
        delegations: Vec::new(),
        generated_at: OffsetDateTime::now_utc(),
    })
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

    #[test]
    fn old_snapshot_without_user_prompts_deserializes() {
        let value = serde_json::json!({
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
}
