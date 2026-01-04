//! Defines the generic storage interface for session persistence.

use crate::model::AgentMessage;
use async_trait::async_trait;
use querymt::error::LLMError;
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

/// A generic, asynchronous trait for storing and retrieving chat sessions.
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

    /// Retrieves the rich agent history (including snapshots, reasoning, etc.).
    async fn get_history(&self, session_id: &str) -> Result<Vec<AgentMessage>, LLMError>;

    /// Appends a rich agent message to the session.
    async fn add_message(&self, session_id: &str, message: AgentMessage) -> Result<(), LLMError>;

    /// Forks a session from a specific point in history, creating a deep copy of the messages.
    async fn fork_session(
        &self,
        source_session_id: &str,
        target_message_id: &str,
    ) -> Result<String, LLMError>;
}
