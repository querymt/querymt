use crate::session::store::{Session, SessionStore};
use querymt::{
    chat::{ChatMessage, ChatResponse},
    error::LLMError,
    LLMProvider,
};
use std::sync::{Arc, RwLock};

/// A wrapper around an `LLMProvider` that persists chat interactions to a `SessionStore`.
pub struct SessionProvider {
    inner: Arc<dyn LLMProvider>,
    store: Arc<dyn SessionStore>,
}

impl SessionProvider {
    pub fn new(provider: Box<dyn LLMProvider>, store: Arc<dyn SessionStore>) -> Self {
        Self {
            inner: provider.into(),
            store,
        }
    }

    /// Fetch an existing session by ID
    pub async fn get_session(&self, session_id: &str) -> Result<Option<Session>, LLMError> {
        self.store.get_session(session_id).await
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
        let session = self.store.create_session(None).await?;
        SessionContext::new(Arc::new(self.clone()), session).await
    }
}

impl Clone for SessionProvider {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            store: Arc::clone(&self.store),
        }
    }
}

pub struct SessionContext {
    provider: Arc<SessionProvider>,
    session: Session,
    history: Arc<RwLock<Vec<ChatMessage>>>,
}

impl SessionContext {
    async fn new(provider: Arc<SessionProvider>, session: Session) -> Result<Self, LLMError> {
        let history = provider.store.get_history(&session.id).await?;
        Ok(Self {
            provider,
            session,
            history: Arc::new(RwLock::new(history)),
        })
    }

    /// Get the session information
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Get the session history
    pub async fn history(&self) -> Vec<ChatMessage> {
        self.history.read().unwrap().clone()
    }

    /// Call tool
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, LLMError> {
        self.provider.clone().inner.call_tool(name, args).await
    }

    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        let current_history = {
            let history_guard = self.history.read()?;
            let mut full_history = history_guard.clone();
            full_history.extend_from_slice(messages);
            full_history
        };

        let response = self
            .provider
            .inner
            .chat(&current_history.as_slice())
            .await?;

        // Atomically log the user message and assistant response.
        self.provider
            .store
            .log_exchange(&self.session.id, messages, response.as_ref())
            .await?;

        {
            let mut history_guard = self.history.write()?;
            history_guard.extend_from_slice(messages);
            history_guard.push(response.as_ref().into());
        }

        Ok(response)
    }
}
