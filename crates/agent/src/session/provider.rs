use crate::model::{AgentMessage, MessagePart};
use crate::model_info::get_model_info;
use crate::session::error::{SessionError, SessionResult};
use crate::session::store::{LLMConfig, Session, SessionStore};
use querymt::LLMParams;
use querymt::plugin::host::PluginRegistry;
use querymt::providers::ModelPricing;
use querymt::{
    LLMProvider,
    chat::{ChatMessage, ChatResponse, MessageType},
    error::LLMError,
};
use std::sync::Arc;

/// A wrapper around a `SessionStore` that resolves providers dynamically.
pub struct SessionProvider {
    plugin_registry: Arc<PluginRegistry>,
    history_store: Arc<dyn SessionStore>,
    initial_config: LLMParams,
}

impl SessionProvider {
    pub fn new(
        plugin_registry: Arc<PluginRegistry>,
        store: Arc<dyn SessionStore>,
        initial_config: LLMParams,
    ) -> Self {
        Self {
            plugin_registry,
            history_store: store,
            initial_config,
        }
    }

    /// Fetch an existing session by ID
    pub async fn get_session(&self, session_id: &str) -> SessionResult<Option<Session>> {
        self.history_store.get_session(session_id).await
    }

    /// Load an existing session by ID
    pub async fn with_session(&self, session_id: &str) -> SessionResult<SessionContext> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| SessionError::SessionNotFound(session_id.to_string()))?;
        SessionContext::new(Arc::new(self.clone()), session).await
    }

    /// Create a new session with optional cwd
    pub async fn create_session(
        &self,
        cwd: Option<std::path::PathBuf>,
    ) -> SessionResult<SessionContext> {
        let mut session = self.history_store.create_session(None, cwd).await?;
        let llm_config = self
            .history_store
            .create_or_get_llm_config(&self.initial_config)
            .await?;
        self.history_store
            .set_session_llm_config(&session.public_id, llm_config.id)
            .await?;
        session.llm_config_id = Some(llm_config.id);
        SessionContext::new(Arc::new(self.clone()), session).await
    }

    pub fn history_store(&self) -> Arc<dyn SessionStore> {
        self.history_store.clone()
    }

    pub fn plugin_registry(&self) -> Arc<PluginRegistry> {
        self.plugin_registry.clone()
    }

    pub async fn build_provider_for_session(
        &self,
        session_id: &str,
    ) -> SessionResult<Arc<dyn LLMProvider>> {
        let llm_config = self
            .history_store
            .get_session_llm_config(session_id)
            .await?
            .ok_or_else(|| {
                SessionError::InvalidOperation("Session has no LLM config".to_string())
            })?;
        self.build_provider(&llm_config).await
    }

    async fn build_provider(&self, config: &LLMConfig) -> SessionResult<Arc<dyn LLMProvider>> {
        let factory = self
            .plugin_registry
            .get(&config.provider)
            .await
            .ok_or_else(|| {
                SessionError::InvalidOperation(format!("Unknown provider: {}", config.provider))
            })?;
        let mut builder_config = serde_json::json!({ "model": config.model });
        if let Some(params) = &config.params
            && let Some(obj) = params.as_object()
        {
            for (key, value) in obj {
                builder_config[key] = value.clone();
            }
        }

        // Get API key - try OAuth first (if feature enabled), then fall back to env var
        if let Some(http_factory) = factory.as_http()
            && let Some(env_var_name) = http_factory.api_key_name()
        {
            let api_key = {
                #[cfg(feature = "oauth")]
                {
                    use crate::auth::get_or_refresh_token;

                    log::debug!("Resolving API key for provider: {}", config.provider);

                    // Try OAuth tokens first
                    match get_or_refresh_token(&config.provider).await {
                        Ok(token) => {
                            log::debug!("Using OAuth token for provider: {}", config.provider);
                            Some(token)
                        }
                        Err(e) => {
                            // OAuth failed - fall back to environment variable
                            log::debug!("OAuth unavailable for {}: {}", config.provider, e);
                            log::debug!("Falling back to env var: {}", env_var_name);
                            std::env::var(&env_var_name).ok()
                        }
                    }
                }
                #[cfg(not(feature = "oauth"))]
                {
                    std::env::var(&env_var_name).ok()
                }
            };

            if let Some(key) = api_key {
                builder_config["api_key"] = key.into();
            } else {
                // Both OAuth and env var failed
                log::warn!(
                    "No API key found for provider '{}'. Set {} or run 'qmt auth login {}'",
                    config.provider,
                    env_var_name,
                    config.provider
                );
            }
        }

        let provider = factory.from_config(&builder_config)?;
        Ok(Arc::from(provider))
    }

    /// Get pricing information for a session's model
    ///
    /// Returns `None` if:
    /// - The session doesn't have an LLM config
    /// - Pricing information is not available for the model
    pub async fn get_session_pricing(
        &self,
        session_id: &str,
    ) -> SessionResult<Option<ModelPricing>> {
        let llm_config = self
            .history_store
            .get_session_llm_config(session_id)
            .await?;

        Ok(llm_config
            .and_then(|config| get_model_info(&config.provider, &config.model))
            .map(|info| info.pricing))
    }

    /// Get pricing information for a specific provider and model
    pub fn get_pricing(provider: &str, model: &str) -> Option<ModelPricing> {
        get_model_info(provider, model).map(|info| info.pricing)
    }
}

impl Clone for SessionProvider {
    fn clone(&self) -> Self {
        Self {
            plugin_registry: self.plugin_registry.clone(),
            history_store: Arc::clone(&self.history_store),
            initial_config: self.initial_config.clone(),
        }
    }
}

pub struct SessionContext {
    provider: Arc<SessionProvider>,
    session: Session,
}

impl SessionContext {
    pub async fn new(provider: Arc<SessionProvider>, session: Session) -> SessionResult<Self> {
        Ok(Self { provider, session })
    }

    /// Get the session information
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub async fn provider(&self) -> SessionResult<Arc<dyn LLMProvider>> {
        self.provider
            .build_provider_for_session(&self.session.public_id)
            .await
    }

    /// Get the session history as rich AgentMessages
    pub async fn get_agent_history(&self) -> SessionResult<Vec<AgentMessage>> {
        self.provider
            .history_store
            .get_history(&self.session.public_id)
            .await
    }

    /// Get the session history converted to standard ChatMessages for the LLM
    pub async fn history(&self) -> Vec<ChatMessage> {
        match self.get_agent_history().await {
            Ok(agent_msgs) => {
                let start_index = agent_msgs
                    .iter()
                    .rposition(|m| {
                        m.parts
                            .iter()
                            .any(|p| matches!(p, MessagePart::Compaction { .. }))
                    })
                    .unwrap_or(0);
                agent_msgs[start_index..]
                    .iter()
                    .map(|m| m.to_chat_message())
                    .collect()
            }
            Err(err) => {
                log::warn!("Failed to load session history: {}", err);
                Vec::new()
            }
        }
    }

    /// Persist an AgentMessage to the store
    pub async fn add_message(&self, message: AgentMessage) -> SessionResult<()> {
        self.provider
            .history_store
            .add_message(&self.session.public_id, message)
            .await
    }

    /// Execute a raw tool call without side effects
    pub async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<String, LLMError> {
        let provider = self.provider().await?;
        provider.call_tool(name, args).await
    }

    /// Submit messages to the LLM without auto-saving
    pub async fn submit_request(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let provider = self.provider().await?;
        provider.chat(messages).await
    }

    /// Higher-level chat interface (used by CLI) that handles conversion and storage
    pub async fn chat(&self, messages: &[ChatMessage]) -> SessionResult<Box<dyn ChatResponse>> {
        // 1. Store incoming messages (User or Tool Result)
        for msg in messages {
            let agent_msg = self.convert_chat_to_agent(msg);
            self.add_message(agent_msg).await?;
        }

        // 2. Fetch full history for context
        let llm_messages = self.history().await;

        // 3. Call LLM
        let response = self.submit_request(&llm_messages).await?;

        // 4. Store response
        let response_msg: ChatMessage = response.as_ref().into();
        let agent_response = self.convert_chat_to_agent(&response_msg);
        self.add_message(agent_response).await?;

        Ok(response)
    }

    /// Get pricing information for this session's model
    pub async fn get_pricing(&self) -> SessionResult<Option<ModelPricing>> {
        self.provider
            .get_session_pricing(&self.session.public_id)
            .await
    }

    pub fn convert_chat_to_agent(&self, msg: &ChatMessage) -> AgentMessage {
        let mut parts = Vec::new();

        match &msg.message_type {
            MessageType::Text => {
                parts.push(MessagePart::Text {
                    content: msg.content.clone(),
                });
            }
            MessageType::ToolUse(calls) => {
                if !msg.content.is_empty() {
                    parts.push(MessagePart::Text {
                        content: msg.content.clone(),
                    });
                }
                for call in calls {
                    parts.push(MessagePart::ToolUse(call.clone()));
                }
            }
            MessageType::ToolResult(calls) => {
                for (i, call) in calls.iter().enumerate() {
                    parts.push(MessagePart::ToolResult {
                        call_id: call.id.clone(),
                        content: if i == 0 {
                            msg.content.clone()
                        } else {
                            "(See previous result)".to_string()
                        },
                        is_error: false,
                        tool_name: Some(call.function.name.clone()),
                        tool_arguments: Some(call.function.arguments.clone()),
                    });
                }
            }
            _ => {
                parts.push(MessagePart::Text {
                    content: msg.content.clone(),
                });
            }
        }

        AgentMessage {
            id: uuid::Uuid::now_v7().to_string(),
            session_id: self.session.public_id.clone(),
            role: msg.role.clone(),
            parts,
            created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            parent_message_id: None,
        }
    }
}
