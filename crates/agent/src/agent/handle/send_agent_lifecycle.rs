use super::*;

impl LocalAgentHandle {
    pub(super) async fn handle_initialize(
        &self,
        req: InitializeRequest,
    ) -> Result<InitializeResponse, Error> {
        use crate::acp::protocol::{
            AgentCapabilities, Implementation, McpCapabilities, PromptCapabilities,
            ProtocolVersion, SessionCapabilities, SessionCloseCapabilities,
            SessionDeleteCapabilities, SessionForkCapabilities, SessionListCapabilities,
            SessionResumeCapabilities,
        };

        let protocol_version = if req.protocol_version <= ProtocolVersion::LATEST {
            req.protocol_version
        } else {
            ProtocolVersion::LATEST
        };

        if let Ok(mut state) = self.client_state.lock() {
            *state = Some(ClientState {
                protocol_version,
                client_capabilities: req.client_capabilities.clone(),
                client_info: req.client_info.clone(),
                authenticated: false,
            });
        }

        let auth_methods = self.config.auth_methods.clone();

        let mut capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().embedded_context(true))
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true))
            .session_capabilities(
                SessionCapabilities::new()
                    .list(SessionListCapabilities::new())
                    .fork(SessionForkCapabilities::new())
                    .resume(SessionResumeCapabilities::new())
                    .close(SessionCloseCapabilities::new())
                    .delete(SessionDeleteCapabilities::new()),
            );

        // Add delegation metadata if agent registry is available
        if let Some(delegation_meta) = self.build_delegation_meta() {
            capabilities = capabilities.meta(delegation_meta);
        }

        Ok(InitializeResponse::new(protocol_version)
            .agent_capabilities(capabilities)
            .auth_methods(auth_methods)
            .agent_info(
                Implementation::new("querymt-agent", env!("QMT_BUILD_VERSION"))
                    .title("QueryMT Agent"),
            ))
    }

    pub(super) async fn handle_authenticate(
        &self,
        req: AuthenticateRequest,
    ) -> Result<AuthenticateResponse, Error> {
        let auth_methods = &self.config.auth_methods;

        if !auth_methods.is_empty() && !auth_methods.iter().any(|m| *m.id() == req.method_id) {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown auth method",
                "methodId": req.method_id.to_string(),
            })));
        }

        if let Ok(mut state) = self.client_state.lock()
            && let Some(state) = state.as_mut()
        {
            state.authenticated = true;
        }
        Ok(AuthenticateResponse::new())
    }

    pub(super) async fn handle_list_sessions(
        &self,
        req: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, Error> {
        let view_store = self
            .config
            .storage
            .view_store()
            .ok_or_else(|| Error::internal_error().data("view store unavailable"))?;
        let page = crate::api::AgentSessions::list_for_acp_with_runtime(self, view_store, req)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;
        Ok(ListSessionsResponse::new(page.sessions).next_cursor(page.next_cursor))
    }

    pub(super) async fn handle_fork_session(
        &self,
        req: ForkSessionRequest,
    ) -> Result<ForkSessionResponse, Error> {
        // Phase 1: Prepare fork (heavy DB work, NO registry lock held)
        let response = self.session_materializer.prepare_fork_session(req).await?;

        // The forked session is created in DB but not materialized yet.
        // The client will need to load it separately (which will use the 3-phase pattern).
        Ok(response)
    }

    pub(super) async fn handle_resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        // Single-flight check: Check if session is already materialized
        // Clone the actor_ref out of the registry to avoid holding the lock
        // during the async ask(GetMode) call.
        let existing_actor_ref = {
            let registry = self.registry_lock().await;
            registry.local_actor_ref(&session_id).cloned()
        };

        if let Some(existing_ref) = existing_actor_ref {
            // Registry lock is now dropped, safe to make async actor call
            let current_mode = existing_ref
                .ask(crate::agent::messages::GetMode)
                .mailbox_timeout(std::time::Duration::from_secs(10))
                .reply_timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))?;

            let config_options = self
                .session_config_options(
                    Some(&session_id),
                    current_mode,
                    **self.config.default_reasoning_effort.load(),
                )
                .await?;
            return Ok(ResumeSessionResponse::new()
                .modes(crate::agent::session_registry::mode_state(current_mode))
                .config_options(config_options));
        }

        // Phase 1: Prepare session (heavy work, NO registry lock held)
        // The session_materializer's internal single-flight lock prevents
        // duplicate materialization of the same session ID.
        // Pass registry so it can re-check after acquiring the lock.
        let (prepared, session_ref) = match self
            .session_materializer
            .prepare_resume_session(req, Some(&self.registry))
            .await?
        {
            PreparedSessionResult::Prepared(prepared) => {
                let session_ref = {
                    let mut registry = self.registry_lock().await;
                    registry.register_prepared_session(&prepared).await
                };
                (Some(prepared), session_ref)
            }
            PreparedSessionResult::AlreadyRegistered(session_ref) => (None, session_ref),
        };

        // Phase 3: Finalize session (post-registration work, NO registry lock held)
        // Pass the bridge for setup outside the lock
        if let Some(prepared) = prepared.as_ref() {
            let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
            self.session_materializer
                .finalize_session(prepared, bridge)
                .await?;
        }

        // Get current mode for response
        let current_mode = session_ref.get_mode().await.map_err(Error::from)?;

        let config_options = self
            .session_config_options(
                Some(&session_id),
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )
            .await?;

        Ok(ResumeSessionResponse::new()
            .modes(crate::agent::session_registry::mode_state(current_mode))
            .config_options(config_options))
    }

    pub(super) async fn handle_close_session(
        &self,
        req: CloseSessionRequest,
    ) -> Result<CloseSessionResponse, Error> {
        let session_id = req.session_id.to_string();
        let result = self.stop_session(&session_id).await;
        self.clear_delegate_model_overrides(&session_id).await;
        result?;
        Ok(CloseSessionResponse::new())
    }

    pub(super) async fn handle_delete_session(
        &self,
        req: DeleteSessionRequest,
    ) -> Result<DeleteSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        let is_loaded = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).is_some()
        };
        if is_loaded {
            // Closing is best-effort: deleting persisted history is the primary intent.
            let _ = self.stop_session(&session_id).await;
        }

        let exists = self
            .config
            .provider
            .history_store()
            .get_session(&session_id)
            .await
            .map_err(|e| Error::internal_error().data(serde_json::json!({"error": e.to_string()})))?
            .is_some();
        if !exists {
            self.clear_delegate_model_overrides(&session_id).await;
            return Ok(DeleteSessionResponse::new());
        }

        self.config
            .provider
            .history_store()
            .delete_session(&session_id)
            .await
            .map_err(|e| {
                Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
            })?;
        self.clear_delegate_model_overrides(&session_id).await;

        Ok(DeleteSessionResponse::new())
    }

    pub(crate) async fn clear_delegate_model_overrides(&self, session_id: &str) {
        self.config
            .delegate_model_overrides
            .clear_parent(session_id)
            .await;

        let Some(profiles) = self.profiles() else {
            return;
        };
        if let Some(binding) = profiles.session_binding(session_id).await
            && let Ok(runtime) = profiles.runtime_for_profile(&binding.profile_id).await
        {
            runtime
                .agent()
                .handle()
                .config
                .delegate_model_overrides
                .clear_parent(session_id)
                .await;
        }
    }
}
