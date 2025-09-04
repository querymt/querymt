//! Defines the generic storage interface for session persistence.

use async_trait::async_trait;
use querymt::{
    chat::{ChatMessage, ChatResponse, ChatRole},
    error::LLMError,
    ToolCall, Usage,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Represents the metadata for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub created_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
}

/// Represents a single message stored within a session, including all metadata.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: String,
    pub session_id: String,
    pub timestamp: OffsetDateTime,
    pub role: ChatRole,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub usage: Option<Usage>,
}

/// Represents a search result, linking a message to its session.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub session: Session,
    pub message: StoredMessage,
    /// A relevance score provided by the search engine.
    pub score: f32,
}

/// A generic, asynchronous trait for storing and retrieving chat sessions.
///
/// This abstraction allows for different database backends (e.g., SQLite, PostgreSQL)
/// to be used for session persistence.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Creates a new session, optionally with a name.
    async fn create_session(&self, name: Option<String>) -> Result<Session, LLMError>;

    /// Retrieves session metadata by its unique ID.
    async fn get_session(&self, session_id: &str) -> Result<Option<Session>, LLMError>;

    /// Lists all available sessions.
    async fn list_sessions(&self) -> Result<Vec<Session>, LLMError>;

    /// Deletes a session and all its associated data.
    async fn delete_session(&self, session_id: &str) -> Result<(), LLMError>;

    /// Logs a user message and the corresponding assistant response to the session history.
    async fn log_exchange(
        &self,
        session_id: &str,
        user_messages: &[ChatMessage],
        assistant_response: &dyn ChatResponse,
    ) -> Result<(), LLMError>;

    /// Retrieves the complete history of a session as a list of `ChatMessage` objects,
    /// ready to be sent to an LLM for continuation.
    async fn get_history(&self, session_id: &str) -> Result<Vec<ChatMessage>, LLMError>;

    /// Performs a full-text search across all messages in all sessions.
    async fn search(&self, query: &str) -> Result<Vec<SearchResult>, LLMError>;
}
