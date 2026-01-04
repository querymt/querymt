//! Error types for session and repository operations

use querymt::error::LLMError;
use thiserror::Error;

/// Errors that can occur during session operations
#[derive(Debug, Error)]
pub enum SessionError {
    /// Session not found
    #[error("Session not found: {0}")]
    SessionNotFound(String),

    /// Task not found
    #[error("Task not found: {0}")]
    TaskNotFound(String),

    /// Intent snapshot not found
    #[error("Intent snapshot not found: {0}")]
    IntentSnapshotNotFound(String),

    /// Decision not found
    #[error("Decision not found: {0}")]
    DecisionNotFound(String),

    /// Alternative not found
    #[error("Alternative not found: {0}")]
    AlternativeNotFound(String),

    /// Progress entry not found
    #[error("Progress entry not found: {0}")]
    ProgressEntryNotFound(String),

    /// Artifact not found
    #[error("Artifact not found: {0}")]
    ArtifactNotFound(String),

    /// Delegation not found
    #[error("Delegation not found: {0}")]
    DelegationNotFound(String),

    /// Invalid fork point reference
    #[error("Invalid fork point reference: {0}")]
    InvalidForkPoint(String),

    /// Fork point type mismatch
    #[error("Fork point type mismatch: expected {expected}, got {actual}")]
    ForkPointTypeMismatch { expected: String, actual: String },

    /// Invalid message index
    #[error("Invalid message index: {0}")]
    InvalidMessageIndex(String),

    /// Database error
    #[error("Database error: {0}")]
    DatabaseError(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Invalid operation
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),

    /// Provider error (from LLM operations)
    #[error("Provider error: {0}")]
    ProviderError(#[from] LLMError),

    /// Generic error
    #[error("{0}")]
    Other(String),
}

/// Convenience type alias for Result with SessionError
pub type SessionResult<T> = Result<T, SessionError>;

impl From<rusqlite::Error> for SessionError {
    fn from(err: rusqlite::Error) -> Self {
        SessionError::DatabaseError(err.to_string())
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(err: serde_json::Error) -> Self {
        SessionError::SerializationError(err.to_string())
    }
}

// Allow converting SessionError to LLMError for backwards compatibility
impl From<SessionError> for LLMError {
    fn from(err: SessionError) -> Self {
        match err {
            SessionError::SessionNotFound(msg)
            | SessionError::TaskNotFound(msg)
            | SessionError::IntentSnapshotNotFound(msg)
            | SessionError::DecisionNotFound(msg)
            | SessionError::AlternativeNotFound(msg)
            | SessionError::ProgressEntryNotFound(msg)
            | SessionError::ArtifactNotFound(msg)
            | SessionError::DelegationNotFound(msg) => LLMError::InvalidRequest(msg),
            SessionError::InvalidForkPoint(msg)
            | SessionError::InvalidMessageIndex(msg)
            | SessionError::InvalidOperation(msg) => LLMError::InvalidRequest(msg),
            SessionError::ForkPointTypeMismatch { expected, actual } => {
                LLMError::InvalidRequest(format!(
                    "Fork point type mismatch: expected {}, got {}",
                    expected, actual
                ))
            }
            SessionError::DatabaseError(msg) => LLMError::ProviderError(msg),
            SessionError::SerializationError(msg) => LLMError::ProviderError(msg),
            SessionError::ProviderError(e) => e,
            SessionError::Other(msg) => LLMError::ProviderError(msg),
        }
    }
}
