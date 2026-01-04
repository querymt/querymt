use async_trait::async_trait;
use querymt::error::LLMError;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::events::{AgentEvent, EventObserver};
use crate::session::store::SessionStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggedEvent {
    pub seq: u64,
    pub timestamp: i64,
    pub kind: crate::events::AgentEventKind,
}

pub struct SessionLogger {
    store: Arc<dyn SessionStore>,
}

impl SessionLogger {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl EventObserver for SessionLogger {
    async fn on_event(&self, event: &AgentEvent) -> Result<(), LLMError> {
        let payload = serde_json::to_string(&LoggedEvent {
            seq: event.seq,
            timestamp: event.timestamp,
            kind: event.kind.clone(),
        })
        .map_err(|e| LLMError::ProviderError(format!("serialize failed: {}", e)))?;

        let message = crate::model::AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: event.session_id.clone(),
            role: querymt::chat::ChatRole::Assistant,
            parts: vec![crate::model::MessagePart::Snapshot {
                root_hash: event.seq.to_string(),
                diff_summary: Some(payload),
            }],
            created_at: event.timestamp,
            parent_message_id: None,
        };

        self.store.add_message(&event.session_id, message).await
    }
}
