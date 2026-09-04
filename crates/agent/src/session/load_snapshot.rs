use crate::acp::protocol::ContentBlock;
use crate::agent::LocalAgentHandle;
use crate::events::AgentEvent;
use crate::model::MessagePart;
use crate::session::error::SessionResult;
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
                MessagePart::Prompt { blocks } => Some(blocks.clone()),
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
    let is_remote_attached = {
        let registry = agent.registry.lock().await;
        registry.get(session_id).is_some_and(|r| r.is_remote())
    };

    // Load the same snapshot the web UI uses. Remote attached sessions may not
    // have a full local projection row yet, so fall back to journal events.
    let audit = match view_store.get_audit_view(session_id, false).await {
        Ok(audit) => audit,
        Err(e) if is_remote_attached => {
            let events: Vec<AgentEvent> = agent
                .config
                .event_sink
                .journal()
                .load_session_stream(session_id, None, None)
                .await?
                .into_iter()
                .map(AgentEvent::from)
                .collect();

            tracing::debug!(
                session_id,
                error = %e,
                event_count = events.len(),
                "remote session missing local audit projection; loaded journal-backed snapshot"
            );

            AuditView {
                session_id: session_id.to_string(),
                events,
                tasks: Vec::new(),
                intent_snapshots: Vec::new(),
                decisions: Vec::new(),
                progress_entries: Vec::new(),
                artifacts: Vec::new(),
                delegations: Vec::new(),
                generated_at: OffsetDateTime::now_utc(),
            }
        }
        Err(e) => return Err(e),
    };

    let cursor = cursor_from_events(&audit.events);
    let delegation_updates =
        crate::control::delegation_notifications::delegation_updates_from_events(&audit.events);
    let messages = agent
        .config
        .provider
        .history_store()
        .get_history(session_id)
        .await?;
    let user_prompts = Some(user_prompt_records(&messages));
    Ok(SessionLoadSnapshot {
        audit,
        cursor,
        delegation_updates,
        user_prompts,
    })
}

#[cfg(test)]
mod tests {
    use super::{SessionLoadSnapshot, load_session_snapshot, user_prompt_records};
    use crate::acp::protocol::{ContentBlock, ImageContent, TextContent};
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::model::{AgentMessage, MessagePart};
    use crate::session::backend::StorageBackend;
    use crate::session::error::SessionError;
    use crate::session::provider::SessionProvider;
    use crate::session::store::SessionStore;
    use crate::test_utils::{MockSessionStore, empty_plugin_registry};
    use querymt::{LLMParams, chat::ChatRole};
    use std::sync::Arc;

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
}
