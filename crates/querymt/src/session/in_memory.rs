use super::{Session, SessionEntry, SessionId, SessionStore, SessionStoreError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sigstore::rekor::models::search_log_query;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// An in-memory implementation of the `SessionStore` trait.
pub struct InMemorySessionStore {
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
}

impl InMemorySessionStore {
    /// Creates a new `InMemorySessionStore`.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStore for InMemorySessionStore {
    async fn create_session(&self, session: Session) -> Result<(), SessionStoreError> {
        let mut sessions = self.sessions.lock().await;
        if sessions
            .insert(session.id.clone(), session.clone())
            .is_some()
        {
            Err(SessionStoreError::AlreadyExists(
                sessions.get_key_value(&session.id).unwrap().0.clone(),
            ))
        } else {
            Ok(())
        }
    }

    async fn get_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Session>, SessionStoreError> {
        let sessions = self.sessions.lock().await;
        Ok(sessions.get(session_id).cloned())
    }

    async fn add_session_entry(
        &self,
        session_id: &SessionId,
        entry: SessionEntry,
    ) -> Result<(), SessionStoreError> {
        let mut sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get_mut(session_id) {
            session.add_entry(entry);
            session.updated_at = Utc::now(); // Ensure updated_at is current
            Ok(())
        } else {
            Err(SessionStoreError::NotFound(session_id.clone()))
        }
    }

    async fn update_session(&self, session: &Session) -> Result<(), SessionStoreError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&session.id) {
            sessions.insert(session.id.clone(), session.clone());
            Ok(())
        } else {
            Err(SessionStoreError::NotFound(session.id.clone()))
        }
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), SessionStoreError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.remove(session_id).is_some() {
            Ok(())
        } else {
            Err(SessionStoreError::NotFound(session_id.clone()))
        }
    }
    /// Searches for session entries matching a full-text query within a specific session.
    async fn search_session_entries(
        &self,
        session_id: &SessionId,
        query: &str,
    ) -> Result<Vec<(DateTime<Utc>, SessionEntry)>, SessionStoreError> {
        unimplemented!()
    }

    /// Searches for session entries across all sessions matching a full-text query.
    async fn search_all_session_entries(
        &self,
        query: &str,
    ) -> Result<Vec<(SessionId, DateTime<Utc>, SessionEntry)>, SessionStoreError> {
        unimplemented!()
    }
}
