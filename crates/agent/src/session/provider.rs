use crate::model::{AgentMessage, MessagePart};
use crate::session::store::{Session, SessionStore};
use querymt::{
    LLMProvider,
    chat::{ChatMessage, ChatResponse, MessageType},
    error::LLMError,
};
use std::sync::Arc;

/// A wrapper around an `LLMProvider` that manages session history via a `SessionStore`.
pub struct SessionProvider {
    inner: Arc<dyn LLMProvider>,
    history_store: Arc<dyn SessionStore>,
}

impl SessionProvider {
    pub fn new(provider: Arc<dyn LLMProvider>, store: Arc<dyn SessionStore>) -> Self {
        Self {
            inner: provider,
            history_store: store,
        }
    }

    /// Fetch an existing session by ID
    pub async fn get_session(&self, session_id: &str) -> Result<Option<Session>, LLMError> {
        self.history_store.get_session(session_id).await
    }

    /// Create or get a session context for operations
    pub async fn with_session(
        &self,
        session_id: Option<String>,
    ) -> Result<SessionContext, LLMError> {
        if let Some(sid) = session_id {
            match self.get_session(&sid).await? {
                Some(session) => return SessionContext::new(Arc::new(self.clone()), session).await,
                _ => (),
            }
        }
        let session = self.history_store.create_session(None).await?;
        SessionContext::new(Arc::new(self.clone()), session).await
    }

    pub fn history_store(&self) -> Arc<dyn SessionStore> {
        self.history_store.clone()
    }

    pub fn llm(&self) -> Arc<dyn LLMProvider> {
        self.inner.clone()
    }
}

impl Clone for SessionProvider {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            history_store: Arc::clone(&self.history_store),
        }
    }
}

pub struct SessionContext {
    provider: Arc<SessionProvider>,
    session: Session,
}

impl SessionContext {
    pub async fn new(provider: Arc<SessionProvider>, session: Session) -> Result<Self, LLMError> {
        Ok(Self { provider, session })
    }

    /// Get the session information
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn provider(&self) -> Arc<dyn LLMProvider> {
        self.provider.inner.clone()
    }

    /// Get the session history as rich AgentMessages
    pub async fn get_agent_history(&self) -> Result<Vec<AgentMessage>, LLMError> {
        self.provider
            .history_store
            .get_history(&self.session.id)
            .await
    }

    /// Get the session history converted to standard ChatMessages for the LLM
    pub async fn history(&self) -> Vec<ChatMessage> {
        match self.get_agent_history().await {
            Ok(agent_msgs) => {
                let start_index = agent_msgs
                    .iter()
                    .rposition(|m| {
                        m.parts
                            .iter()
                            .any(|p| matches!(p, MessagePart::Compaction { .. }))
                    })
                    .unwrap_or(0);
                agent_msgs[start_index..]
                    .iter()
                    .map(|m| m.to_chat_message())
                    .collect()
            }
            Err(err) => {
                log::warn!("Failed to load session history: {}", err);
                Vec::new()
            }
        }
    }

    /// Persist an AgentMessage to the store
    pub async fn add_message(&self, message: AgentMessage) -> Result<(), LLMError> {
        self.provider
            .history_store
            .add_message(&self.session.id, message)
            .await
    }

    /// Execute a raw tool call without side effects
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, LLMError> {
        self.provider.inner.call_tool(name, args).await
    }

    /// Submit messages to the LLM without auto-saving
    pub async fn submit_request(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.provider.inner.chat(messages).await
    }

    /// Higher-level chat interface (used by CLI) that handles conversion and storage
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        // 1. Store incoming messages (User or Tool Result)
        for msg in messages {
            let agent_msg = self.convert_chat_to_agent(msg);
            self.add_message(agent_msg).await?;
        }

        // 2. Fetch full history for context
        let llm_messages = self.history().await;

        // 3. Call LLM
        let response = self.submit_request(&llm_messages).await?;

        // 4. Store response
        let response_msg: ChatMessage = response.as_ref().into();
        let agent_response = self.convert_chat_to_agent(&response_msg);
        self.add_message(agent_response).await?;

        Ok(response)
    }

    pub fn convert_chat_to_agent(&self, msg: &ChatMessage) -> AgentMessage {
        let mut parts = Vec::new();

        match &msg.message_type {
            MessageType::Text => {
                parts.push(MessagePart::Text {
                    content: msg.content.clone(),
                });
            }
            MessageType::ToolUse(calls) => {
                if !msg.content.is_empty() {
                    parts.push(MessagePart::Text {
                        content: msg.content.clone(),
                    });
                }
                for call in calls {
                    parts.push(MessagePart::ToolUse(call.clone()));
                }
            }
            MessageType::ToolResult(calls) => {
                for (i, call) in calls.iter().enumerate() {
                    parts.push(MessagePart::ToolResult {
                        call_id: call.id.clone(),
                        content: if i == 0 {
                            msg.content.clone()
                        } else {
                            "(See previous result)".to_string()
                        },
                        is_error: false,
                        tool_name: Some(call.function.name.clone()),
                        tool_arguments: Some(call.function.arguments.clone()),
                    });
                }
            }
            _ => {
                parts.push(MessagePart::Text {
                    content: msg.content.clone(),
                });
            }
        }

        AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: self.session.id.clone(),
            role: msg.role.clone(),
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        }
    }
}
