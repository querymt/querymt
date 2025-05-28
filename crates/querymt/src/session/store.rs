use super::{Session, SessionEntry, SessionId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// An error type for session store operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionStoreError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Session already exists: {0}")]
    AlreadyExists(SessionId),
    #[error("Database error: {0}")]
    DbError(String),
    #[error("Serialization/Deserialization error: {0}")]
    CodecError(String),
    #[error("Other session store error: {0}")]
    Other(String),
}

/// Trait for abstracting asynchronous session storage operations.
/// Any concrete database backend (Postgres, Redis, MongoDB, etc.) should implement this trait.
#[async_trait]
pub trait SessionStore: Send + Sync + 'static {
    // 'static is often needed for Arc<dyn Trait>
    /// Creates a new session in the store.
    async fn create_session(&self, session: Session) -> Result<(), SessionStoreError>;

    /// Retrieves a session by its ID.
    async fn get_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Session>, SessionStoreError>;

    /// Appends a new entry (message or tool call/result) to an existing session.
    async fn add_session_entry(
        &self,
        session_id: &SessionId,
        entry: SessionEntry,
    ) -> Result<(), SessionStoreError>;

    /// Updates an existing session (e.g., for metadata changes or full replacement).
    async fn update_session(&self, session: &Session) -> Result<(), SessionStoreError>;

    /// Deletes a session by its ID.
    async fn delete_session(&self, session_id: &SessionId) -> Result<(), SessionStoreError>;

    /// Searches for session entries matching a full-text query within a specific session.
    async fn search_session_entries(
        &self,
        session_id: &SessionId,
        query: &str,
    ) -> Result<Vec<(DateTime<Utc>, SessionEntry)>, SessionStoreError>;

    /// Searches for session entries across all sessions matching a full-text query.
    async fn search_all_session_entries(
        &self,
        query: &str,
    ) -> Result<Vec<(SessionId, DateTime<Utc>, SessionEntry)>, SessionStoreError>;
}
