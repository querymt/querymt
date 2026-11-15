use thiserror::Error;

/// Errors that can occur during middleware state transitions
#[derive(Error, Debug)]
pub enum MiddlewareError {
    #[error("State transition failed: {0}")]
    Transition(String),

    #[error("Message injection failed: {0}")]
    Injection(String),

    #[error("Tool execution blocked: {0}")]
    ToolBlocked(String),

    #[error("Compaction failed: {0}")]
    Compaction(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Invalid state: expected {expected}, got {actual}")]
    InvalidState {
        expected: &'static str,
        actual: &'static str,
    },

    #[error("Missing required field: {0}")]
    MissingField(&'static str),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Execution error: {0}")]
    ExecutionError(String),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, MiddlewareError>;
