use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::chat::ChatMessage;
use crate::ToolCall;

mod in_memory;
mod session_provider;
mod sqlite;
mod store;
pub use store::{SessionStore, SessionStoreError};
/// A unique identifier for a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Creates a new, random session ID.
    pub fn new() -> Self {
        SessionId(Uuid::new_v4().to_string())
    }

    /// Creates a session ID from a string.
    pub fn from_str(s: &str) -> Self {
        SessionId(s.to_string())
    }

    /// Returns the inner string representation of the session ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Represents a single event or entry within a conversation session.
/// This enum captures different types of interactions that should be logged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEntry {
    /// A chat message (either from the user, assistant text response, or tool results/invocations fed back to LLM).
    Message(ChatMessage),
    /// An LLM's explicit request to call one or more tools, as parsed from its response.
    ToolCallAttempt(ToolCall),
    /// Records a failure of an LLM provider operation.
    /// (Type of operation, e.g., "chat", "completion", and the error message)
    LLMFailure(String, String),
}

/// Represents a continuous conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// The unique identifier for this session.
    pub id: SessionId,
    /// Timestamp when the session was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the session was last updated with a new entry.
    pub updated_at: DateTime<Utc>,
    /// Chronological list of entries (messages, tool calls) within the session.
    pub entries: Vec<(DateTime<Utc>, SessionEntry)>, // Store events with their exact timestamp
                                                     // You could add more metadata here, e.g.:
                                                     // pub user_id: Option<String>,
                                                     // pub metadata: Option<serde_json::Value>,
                                                     // pub initial_llm_config: Option<serde_json::Value>,
}

impl Session {
    /// Creates a new empty session with a generated ID and current timestamps.
    pub fn new() -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            created_at: now,
            updated_at: now,
            entries: Vec::new(),
        }
    }

    /// Adds a new entry to the session, updating the `updated_at` timestamp.
    pub fn add_entry(&mut self, entry: SessionEntry) {
        self.entries.push((Utc::now(), entry));
        self.updated_at = Utc::now();
    }
}
