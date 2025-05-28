use super::{Session, SessionEntry, SessionId, SessionStore, SessionStoreError};
use crate::{
    chat::{BasicChatProvider, ChatMessage, ChatResponse, Tool, ToolChatProvider},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    LLMProvider,
};
use async_trait::async_trait;
use chrono::Utc;
use log::warn;
use std::sync::Arc;
use tokio::task;

/// A wrapper around an `LLMProvider` that logs all interactions to a `SessionStore`.
///
/// This provider ensures that conversations, tool calls, and completions are
/// recorded as `SessionEntry` events associated with a specific `SessionId`.
/// Logging happens asynchronously in the background to avoid blocking the main response flow.
pub struct SessionLLMProvider<S: SessionStore> {
    inner: Box<dyn LLMProvider>,
    session_id: SessionId,
    session_store: Arc<S>,
}

impl<S: SessionStore> SessionLLMProvider<S> {
    /// Creates a new `SessionLLMProvider`.
    ///
    /// It can either associate with an `existing_session_id` (creating it if not found)
    /// or create a new session if `None` is provided.
    pub async fn new(
        inner: Box<dyn LLMProvider>,
        session_store: Arc<S>,
        existing_session_id: Option<SessionId>,
    ) -> Result<Self, SessionStoreError> {
        let session_id = if let Some(id) = existing_session_id {
            if session_store.get_session(&id).await?.is_none() {
                let new_session = Session {
                    id: id.clone(),
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                    entries: Vec::new(),
                };
                session_store.create_session(new_session).await?;
            }
            id
        } else {
            let new_session = Session::new();
            let id = new_session.id.clone();
            session_store.create_session(new_session).await?;
            id
        };

        Ok(Self {
            inner,
            session_id,
            session_store,
        })
    }

    /// Returns the ID of the session this provider is logging to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Logs a `SessionEntry` to the associated session.
    fn log_session_entry(&self, entry: SessionEntry) {
        let session_store = Arc::clone(&self.session_store);
        let session_id = self.session_id.clone();

        task::spawn(async move {
            if let Err(e) = session_store.add_session_entry(&session_id, entry).await {
                warn!(
                    "Failed to log session entry for session {}: {}",
                    session_id, e
                );
            }
        });
    }

    /// logs an LLM failure entry in the background.
    fn log_llm_failure(&self, operation_type: &str, error_message: &str) {
        let entry = SessionEntry::LLMFailure(operation_type.to_string(), error_message.to_string());
        self.log_session_entry(entry);
    }
}

#[async_trait]
impl<S: SessionStore> BasicChatProvider for SessionLLMProvider<S> {
    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        // Log user messages (input to the LLM)
        for msg in messages {
            self.log_session_entry(SessionEntry::Message(msg.clone()));
        }

        let response_result = self.inner.chat(messages).await;

        match response_result {
            Ok(response) => {
                if let Some(text) = response.text() {
                    self.log_session_entry(SessionEntry::Message(ChatMessage {
                        role: crate::chat::ChatRole::Assistant,
                        message_type: crate::chat::MessageType::Text,
                        content: text,
                    }));
                }
                Ok(response)
            }
            Err(e) => {
                self.log_llm_failure("chat", &e.to_string());
                Err(e)
            }
        }
    }
}

#[async_trait]
impl<S: SessionStore> ToolChatProvider for SessionLLMProvider<S> {
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        // Log incoming messages (user input, tool results being fed back to LLM, etc.)
        for msg in messages {
            self.log_session_entry(SessionEntry::Message(msg.clone()));
        }

        let response_result = self.inner.chat_with_tools(messages, tools).await;

        match response_result {
            Ok(response) => {
                // Log LLM's response: could be text or tool calls asynchronously
                if let Some(tool_calls) = response.tool_calls() {
                    // Log each tool call requested by the LLM
                    for tool_call in tool_calls {
                        self.log_session_entry(SessionEntry::ToolCallAttempt(tool_call.clone()));
                    }
                } else if let Some(text) = response.text() {
                    // Log assistant's text response
                    self.log_session_entry(SessionEntry::Message(ChatMessage {
                        role: crate::chat::ChatRole::Assistant,
                        message_type: crate::chat::MessageType::Text,
                        content: text,
                    }));
                }
                Ok(response)
            }
            Err(e) => {
                // Log the failure asynchronously
                self.log_llm_failure("chat_with_tools", &e.to_string());
                Err(e) // Re-return the original error
            }
        }
    }
}

#[async_trait]
impl<S: SessionStore> CompletionProvider for SessionLLMProvider<S> {
    async fn complete(&self, req_obj: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        self.log_session_entry(SessionEntry::Message(ChatMessage {
            role: crate::chat::ChatRole::User,
            message_type: crate::chat::MessageType::Text,
            content: format!("Completion Request:\n{}", req_obj.prompt),
        }));

        let response_result = self.inner.complete(req_obj).await;

        match response_result {
            Ok(response) => {
                self.log_session_entry(SessionEntry::Message(ChatMessage {
                    role: crate::chat::ChatRole::Assistant,
                    message_type: crate::chat::MessageType::Text,
                    content: response.text.clone(),
                }));
                Ok(response)
            }
            Err(e) => {
                // Log the failure asynchronously
                self.log_llm_failure("completion", &e.to_string());
                Err(e)
            }
        }
    }
}

#[async_trait]
impl<S: SessionStore> EmbeddingProvider for SessionLLMProvider<S> {
    async fn embed(&self, inputs: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        let result = self.inner.embed(inputs).await;
        if let Err(ref e) = result {
            self.log_llm_failure("embed", &e.to_string());
        }
        result
    }
}

#[async_trait]
impl<S: SessionStore> LLMProvider for SessionLLMProvider<S> {
    fn tools(&self) -> Option<&[Tool]> {
        self.inner.tools()
    }

    // The `call_tool` method represents the actual execution of a tool.
    // The *result* of this execution is typically fed back to the LLM as a `ChatMessage`
    // with `MessageType::ToolResult`. This `ChatMessage` will then be logged
    // by `chat_with_tools` when it's passed as an input message.
    // So, this method itself doesn't need to log.
    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, LLMError> {
        let result = self.inner.call_tool(name, args).await;
        if let Err(ref e) = result {
            self.log_llm_failure(&format!("call_tool: {}", name), &e.to_string());
        }
        result
    }
}
