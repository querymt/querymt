//! Error types for session and repository operations

use querymt::error::LLMError;
use thiserror::Error;

/// Errors that can occur during session operations
#[derive(Debug, Error)]
#[non_exhaustive]
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

    /// Database error; the original rusqlite error is the source.
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// A blocking storage task panicked or was cancelled before finishing.
    #[error("storage task failed")]
    TaskFailed(#[from] tokio::task::JoinError),

    /// Serialization error
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Invalid operation
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),

    /// Session control CAS failed because another writer advanced the revision.
    #[error("session control revision conflict: expected {expected}, found {found}")]
    SessionControlRevisionConflict { expected: u64, found: u64 },

    /// A delegate assignment snapshot was superseded by another writer.
    #[error("delegate assignment revision conflict: expected {expected}, found {found}")]
    DelegateAssignmentRevisionConflict { expected: u64, found: u64 },

    /// Workspace not found in the registry.
    #[error("Workspace not found: {0}")]
    WorkspaceNotFound(String),

    /// A managed workspace is already owned by another writable session.
    #[error("Workspace {workspace} already owned by session {owner}")]
    WorkspaceAlreadyOwned { workspace: String, owner: String },

    /// A managed workspace checkout path is already registered.
    #[error("Workspace checkout path already registered: {0}")]
    WorkspaceCheckoutTaken(String),

    /// Provider error (from LLM operations)
    #[error("Provider error: {0}")]
    ProviderError(#[from] LLMError),

    /// Generic error
    #[error("{0}")]
    Other(String),
}

/// Convenience type alias for Result with SessionError
pub type SessionResult<T> = Result<T, SessionError>;

impl SessionError {
    /// True when the underlying database error is a "no rows" result.
    pub fn is_no_rows(&self) -> bool {
        matches!(
            self,
            SessionError::Database(rusqlite::Error::QueryReturnedNoRows)
        )
    }

    /// True when the underlying database error message contains `needle`.
    ///
    /// Used for errors raised by SQL triggers (`RAISE(ABORT, '...')`), which
    /// carry a marker string rather than a distinct error code.
    pub fn db_message_contains(&self, needle: &str) -> bool {
        matches!(self, SessionError::Database(err) if err.to_string().contains(needle))
    }

    /// True when retrying the operation may succeed: the database was busy
    /// or locked by another connection.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            SessionError::Database(rusqlite::Error::SqliteFailure(failure, _))
                if matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(err: serde_json::Error) -> Self {
        SessionError::SerializationError(err.to_string())
    }
}

/// Formats `err` followed by `: {source}` for each error in its source chain.
fn render_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        // `Database(#[error("Database error: {0}")])` already embeds its source.
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
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
            SessionError::SessionControlRevisionConflict { expected, found } => {
                LLMError::InvalidRequest(format!(
                    "session control revision conflict: expected {expected}, found {found}"
                ))
            }
            SessionError::DelegateAssignmentRevisionConflict { expected, found } => {
                LLMError::InvalidRequest(format!(
                    "delegate assignment revision conflict: expected {expected}, found {found}"
                ))
            }
            SessionError::ForkPointTypeMismatch { expected, actual } => {
                LLMError::InvalidRequest(format!(
                    "Fork point type mismatch: expected {}, got {}",
                    expected, actual
                ))
            }
            SessionError::WorkspaceNotFound(msg) => {
                LLMError::InvalidRequest(format!("Workspace not found: {msg}"))
            }
            SessionError::WorkspaceAlreadyOwned { workspace, owner } => LLMError::InvalidRequest(
                format!("Workspace {workspace} already owned by session {owner}"),
            ),
            SessionError::WorkspaceCheckoutTaken(msg) => {
                LLMError::InvalidRequest(format!("Workspace checkout path taken: {msg}"))
            }
            SessionError::SerializationError(msg) => LLMError::ProviderError(msg),
            SessionError::ProviderError(e) => e,
            SessionError::Other(msg) => LLMError::ProviderError(msg),
            // `LLMError` stores only strings, so render the full source chain.
            err @ (SessionError::Database(_) | SessionError::TaskFailed(_)) => {
                LLMError::ProviderError(render_chain(&err))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn storage_error_converted_for_a_provider_caller_keeps_its_source() {
        let db_err = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("distinctive busy marker".to_string()),
        );
        let err = SessionError::from(db_err);

        // The original rusqlite error is reachable through `source()`.
        let source = err.source().expect("database error has a source");
        assert!(source.downcast_ref::<rusqlite::Error>().is_some());
        assert!(err.is_transient());

        // `LLMError` stores strings only, so the cause must be in its text.
        let llm: LLMError = err.into();
        assert!(
            llm.to_string().contains("distinctive busy marker"),
            "LLM error text lost the rusqlite message: {llm}"
        );
    }

    #[test]
    fn is_transient_only_for_busy_and_locked() {
        let busy = |code| {
            SessionError::from(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
        };
        assert!(busy(rusqlite::ffi::SQLITE_BUSY).is_transient());
        assert!(busy(rusqlite::ffi::SQLITE_LOCKED).is_transient());
        assert!(!busy(rusqlite::ffi::SQLITE_CONSTRAINT).is_transient());
        assert!(!SessionError::from(rusqlite::Error::QueryReturnedNoRows).is_transient());
        assert!(!SessionError::Other("x".into()).is_transient());
    }
}
