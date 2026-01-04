use crate::events::{AgentEvent, AgentEventKind, EventObserver};
use crate::model::{AgentMessage, MessagePart};
use crate::session::store::{Session, SessionStore};
use async_trait::async_trait;
use querymt::chat::ChatRole;
use querymt::error::LLMError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Default)]
struct ProjectionState {
    sessions: HashMap<String, Session>,
    messages: HashMap<String, Vec<AgentMessage>>,
    pending_snapshot: HashMap<String, MessagePart>,
}

pub struct EventProjectionStore {
    state: Arc<Mutex<ProjectionState>>,
}

impl EventProjectionStore {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ProjectionState::default())),
        }
    }

    pub fn spawn_projector(
        &self,
        mut rx: tokio::sync::broadcast::Receiver<AgentEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let store = self.clone();
        tokio::spawn(async move {
            while let Ok(event) = rx.recv().await {
                let _ = store.on_event(&event).await;
            }
        })
    }

    fn push_message(&self, session_id: &str, message: AgentMessage) {
        let mut state = self.state.lock().unwrap();
        state
            .messages
            .entry(session_id.to_string())
            .or_default()
            .push(message);
    }
}

impl Clone for EventProjectionStore {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

#[async_trait]
impl EventObserver for EventProjectionStore {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError> {
        match &event.kind {
            AgentEventKind::SessionCreated => {
                let mut state = self.state.lock().unwrap();
                state
                    .sessions
                    .entry(event.session_id.clone())
                    .or_insert(Session {
                        id: event.session_id.clone(),
                        name: None,
                        created_at: Some(
                            OffsetDateTime::from_unix_timestamp(event.timestamp)
                                .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                        ),
                        updated_at: Some(OffsetDateTime::now_utc()),
                    });
            }
            AgentEventKind::UserMessageStored { content } => {
                self.push_message(
                    &event.session_id,
                    AgentMessage {
                        id: Uuid::new_v4().to_string(),
                        session_id: event.session_id.clone(),
                        role: ChatRole::User,
                        parts: vec![MessagePart::Text {
                            content: content.clone(),
                        }],
                        created_at: event.timestamp,
                        parent_message_id: None,
                    },
                );
            }
            AgentEventKind::AssistantMessageStored { content } => {
                self.push_message(
                    &event.session_id,
                    AgentMessage {
                        id: Uuid::new_v4().to_string(),
                        session_id: event.session_id.clone(),
                        role: ChatRole::Assistant,
                        parts: vec![MessagePart::Text {
                            content: content.clone(),
                        }],
                        created_at: event.timestamp,
                        parent_message_id: None,
                    },
                );
            }
            AgentEventKind::ToolCallStart {
                tool_call_id,
                tool_name,
                arguments,
            } => {
                let tool_use = MessagePart::ToolUse(querymt::ToolCall {
                    id: tool_call_id.clone(),
                    call_type: "function".to_string(),
                    function: querymt::FunctionCall {
                        name: tool_name.clone(),
                        arguments: arguments.clone(),
                    },
                });

                let mut state = self.state.lock().unwrap();
                let messages = state.messages.entry(event.session_id.clone()).or_default();
                if let Some(last) = messages.last_mut() {
                    if last.role == ChatRole::Assistant {
                        last.parts.push(tool_use);
                        return Ok(());
                    }
                }
                messages.push(AgentMessage {
                    id: Uuid::new_v4().to_string(),
                    session_id: event.session_id.clone(),
                    role: ChatRole::Assistant,
                    parts: vec![tool_use],
                    created_at: event.timestamp,
                    parent_message_id: None,
                });
            }
            AgentEventKind::SnapshotEnd { summary } => {
                if let Some(summary) = summary.clone() {
                    let part = MessagePart::Snapshot {
                        root_hash: event.seq.to_string(),
                        diff_summary: Some(summary),
                    };
                    let mut state = self.state.lock().unwrap();
                    state
                        .pending_snapshot
                        .insert(event.session_id.clone(), part);
                }
            }
            AgentEventKind::ToolCallEnd {
                tool_call_id,
                tool_name,
                is_error,
                result,
            } => {
                let mut parts = vec![MessagePart::ToolResult {
                    call_id: tool_call_id.clone(),
                    content: result.clone(),
                    is_error: *is_error,
                    tool_name: Some(tool_name.clone()),
                    tool_arguments: None,
                }];
                let mut state = self.state.lock().unwrap();
                if let Some(snapshot) = state.pending_snapshot.remove(&event.session_id) {
                    parts.push(snapshot);
                }
                state
                    .messages
                    .entry(event.session_id.clone())
                    .or_default()
                    .push(AgentMessage {
                        id: Uuid::new_v4().to_string(),
                        session_id: event.session_id.clone(),
                        role: ChatRole::User,
                        parts,
                        created_at: event.timestamp,
                        parent_message_id: None,
                    });
            }
            AgentEventKind::CompactionEnd { summary, .. } => {
                self.push_message(
                    &event.session_id,
                    AgentMessage {
                        id: Uuid::new_v4().to_string(),
                        session_id: event.session_id.clone(),
                        role: ChatRole::Assistant,
                        parts: vec![MessagePart::Compaction {
                            summary: summary.clone(),
                            original_token_count: 0,
                        }],
                        created_at: event.timestamp,
                        parent_message_id: None,
                    },
                );
            }
            _ => {}
        }
        Ok(())
    }
}

#[async_trait]
impl SessionStore for EventProjectionStore {
    async fn create_session(&self, name: Option<String>) -> Result<Session, LLMError> {
        let session = Session {
            id: Uuid::new_v4().to_string(),
            name,
            created_at: Some(OffsetDateTime::now_utc()),
            updated_at: Some(OffsetDateTime::now_utc()),
        };
        let mut state = self.state.lock().unwrap();
        state.sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    async fn get_session(&self, session_id: &str) -> Result<Option<Session>, LLMError> {
        let state = self.state.lock().unwrap();
        Ok(state.sessions.get(session_id).cloned())
    }

    async fn list_sessions(&self) -> Result<Vec<Session>, LLMError> {
        let state = self.state.lock().unwrap();
        Ok(state.sessions.values().cloned().collect())
    }

    async fn delete_session(&self, session_id: &str) -> Result<(), LLMError> {
        let mut state = self.state.lock().unwrap();
        state.sessions.remove(session_id);
        state.messages.remove(session_id);
        Ok(())
    }

    async fn get_history(&self, session_id: &str) -> Result<Vec<AgentMessage>, LLMError> {
        let state = self.state.lock().unwrap();
        Ok(state.messages.get(session_id).cloned().unwrap_or_default())
    }

    async fn add_message(&self, session_id: &str, message: AgentMessage) -> Result<(), LLMError> {
        let mut state = self.state.lock().unwrap();
        state
            .messages
            .entry(session_id.to_string())
            .or_default()
            .push(message);
        Ok(())
    }

    async fn fork_session(
        &self,
        source_session_id: &str,
        target_message_id: &str,
    ) -> Result<String, LLMError> {
        let mut state = self.state.lock().unwrap();
        let source_messages = state
            .messages
            .get(source_session_id)
            .cloned()
            .unwrap_or_default();
        let mut to_copy = Vec::new();
        for msg in source_messages {
            to_copy.push(msg.clone());
            if msg.id == target_message_id {
                break;
            }
        }
        let new_session_id = Uuid::new_v4().to_string();
        state.sessions.insert(
            new_session_id.clone(),
            Session {
                id: new_session_id.clone(),
                name: Some(format!("Fork of {}", source_session_id)),
                created_at: Some(OffsetDateTime::now_utc()),
                updated_at: Some(OffsetDateTime::now_utc()),
            },
        );
        state.messages.insert(new_session_id.clone(), to_copy);
        Ok(new_session_id)
    }
}
