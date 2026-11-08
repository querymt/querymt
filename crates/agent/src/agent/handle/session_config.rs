use super::*;

impl LocalAgentHandle {
    pub(super) async fn session_reasoning_effort(
        &self,
        session_ref: &crate::agent::remote::SessionActorRef,
        session_id: &str,
    ) -> Option<querymt::chat::ReasoningEffort> {
        match session_ref.get_reasoning_effort().await {
            Ok(effort) => effort,
            Err(err) => {
                tracing::warn!(
                    session_id,
                    error = %err,
                    "failed to get session reasoning effort; using initial config"
                );
                self.config.provider.initial_config().reasoning_effort
            }
        }
    }

    /// Switch provider and model for a session (simple form).
    ///
    /// This is a model selection, not a configuration override: the session
    /// actor keeps the session's system prompt and reasoning effort, seeds the
    /// profile's parameters when the profile's own model is selected, and
    /// leaves a session already bound to this model untouched.
    pub async fn set_provider(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), Error> {
        let session_ref = self.session_ref_for_agent_session(session_id).await?;
        let selection = crate::agent::session_control::SessionModelSelection {
            model_id: format!("{provider}/{model}"),
            provider_node_id: None,
        };
        session_ref
            .set_session_model(selection)
            .await
            .map(|_| ())
            .map_err(Error::from)
    }

    /// Switch provider configuration for a session (advanced form).
    pub async fn set_llm_config(&self, session_id: &str, config: LLMParams) -> Result<(), Error> {
        let provider = config
            .provider
            .as_ref()
            .ok_or_else(|| Error::from(crate::error::AgentError::ProviderRequired))?;
        if self
            .config
            .provider
            .plugin_registry()
            .get(provider)
            .await
            .is_none()
        {
            return Err(Error::from(crate::error::AgentError::UnknownProvider {
                name: provider.clone(),
            }));
        }
        let session_ref = self.session_ref_for_agent_session(session_id).await?;
        session_ref
            .set_llm_config(config, None)
            .await
            .map_err(Error::from)
    }

    /// Get current LLM config for a session
    pub async fn get_session_llm_config(
        &self,
        session_id: &str,
    ) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Get LLM config by ID
    pub async fn get_llm_config(&self, config_id: i64) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_llm_config(config_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Creates a CompositeDriver from the configured middleware drivers
    pub fn create_driver(&self) -> CompositeDriver {
        self.config.create_driver()
    }

    /// Returns the session limits from configured middleware
    pub fn get_session_limits(&self) -> Option<crate::events::SessionLimits> {
        self.config.get_session_limits()
    }

    /// Builds delegation metadata for ACP AgentCapabilities._meta field
    pub fn build_delegation_meta(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        self.config.build_delegation_meta()
    }

    /// Refuse undo/redo for a read-only session (previous owner or archived
    /// workspace): its snapshots are context-only and must never touch a
    /// checkout it no longer owns.
    async fn refuse_if_read_only(
        &self,
        session_id: &str,
    ) -> Result<(), crate::agent::undo::UndoError> {
        self.session_materializer
            .ensure_writable(session_id)
            .await
            .map_err(|err| {
                let field = |key: &str| {
                    err.data
                        .as_ref()
                        .and_then(|d| d.get(key))
                        .and_then(|v| v.as_str())
                };
                crate::agent::undo::UndoError::Other(format!(
                    "{}: {}",
                    field("code").unwrap_or("read_only"),
                    field("message").unwrap_or("session is read-only")
                ))
            })
    }

    /// Undo: revert filesystem to state at the given message_id.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn undo(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<crate::agent::undo::UndoResult, crate::agent::undo::UndoError> {
        self.refuse_if_read_only(session_id).await?;
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.undo(message_id.to_string()).await
    }

    /// Redo: re-apply the next change in the redo stack.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn redo(
        &self,
        session_id: &str,
    ) -> Result<crate::agent::undo::RedoResult, crate::agent::undo::UndoError> {
        self.refuse_if_read_only(session_id).await?;
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.redo().await
    }
}
