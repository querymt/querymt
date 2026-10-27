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
            .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true))
            .session_capabilities(
                SessionCapabilities::new()
                    .list(SessionListCapabilities::new())
                    .fork(SessionForkCapabilities::new())
                    .resume(SessionResumeCapabilities::new())
                    .close(SessionCloseCapabilities::new())
                    .delete(SessionDeleteCapabilities::new()),
            );

        // Additive `_meta`: delegation targets, and workspace support when (and
        // only when) a workspace service is configured.
        let mut meta = self.build_delegation_meta().unwrap_or_default();
        if let Some(service) = self.config.workspace_service() {
            meta.insert(
                crate::acp::shared::QUERYMT_META_NAMESPACE.to_string(),
                serde_json::json!({ "workspace": service.capability_meta() }),
            );
        }
        if !meta.is_empty() {
            capabilities = capabilities.meta(meta);
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
            .map_err(|error| match error {
                crate::api::AcpSessionListError::InvalidCursor => {
                    Error::invalid_params().data("invalid session list cursor")
                }
                crate::api::AcpSessionListError::Backend(error) => {
                    Error::internal_error().data(error.to_string())
                }
            })?;
        Ok(ListSessionsResponse::new(page.sessions).next_cursor(page.next_cursor))
    }

    pub(super) async fn handle_fork_session(
        &self,
        req: ForkSessionRequest,
    ) -> Result<ForkSessionResponse, Error> {
        self.session_materializer
            .ensure_not_transferred(req.session_id.0.as_ref())
            .await?;
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

        // Resolve durable ownership before touching a local actor registry, just
        // as load does. A second runtime would own a competing event counter.
        #[cfg(feature = "remote")]
        if let Some(bookmark) = self
            .config
            .provider
            .history_store()
            .get_remote_session_bookmark(&session_id)
            .await
            .map_err(|error| Error::internal_error().data(error.to_string()))?
        {
            if self
                .config
                .provider
                .history_store()
                .get_session(&session_id)
                .await
                .map_err(|error| Error::internal_error().data(error.to_string()))?
                .is_some()
            {
                return Err(Error::invalid_params().data(serde_json::json!({
                    "message": "Session ID exists both locally and remotely",
                    "sessionId": session_id,
                })));
            }
            self.ensure_remote_session_connected(
                &session_id,
                Some(&bookmark.node_id),
                crate::agent::handle::remote_connect::RemoteConnectReason::Open,
            )
            .await
            .map_err(|error| error.to_acp_error())?;
            let session_ref = self
                .registry
                .lock()
                .await
                .get(&session_id)
                .cloned()
                .ok_or_else(|| Error::internal_error().data("remote session was not attached"))?;
            let current_mode = session_ref.get_mode().await.map_err(Error::from)?;
            return Ok(ResumeSessionResponse::new()
                .modes(crate::agent::session_registry::mode_state(current_mode))
                .config_options(self.remote_session_config_options(&session_id).await?));
        }

        let session_ref =
            if let Some((owner, binding)) = self.bound_session_handle(&session_id).await? {
                let owner = owner
                    .as_any()
                    .downcast_ref::<LocalAgentHandle>()
                    .ok_or_else(|| {
                        Error::internal_error().data(serde_json::json!({
                            "message": "bound session runtime is not local",
                            "sessionId": session_id,
                            "profileId": binding.profile_id,
                            "agentId": binding.agent_id,
                        }))
                    })?;
                let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
                if let Some(bridge) = bridge {
                    owner.set_bridge(bridge).await;
                }
                owner.resume_session_in_runtime(req).await?
            } else {
                if self.profiles().is_some() {
                    return Err(Error::invalid_params().data(serde_json::json!({
                        "message": "Session has no profile binding",
                        "sessionId": session_id,
                    })));
                }
                self.resume_session_in_runtime(req).await?
            };
        let current_mode = session_ref.get_mode().await.map_err(Error::from)?;
        let reasoning_effort = self
            .session_reasoning_effort(&session_ref, &session_id)
            .await;
        let config_options = self
            .session_config_options(Some(&session_id), current_mode, reasoning_effort)
            .await?;
        Ok(ResumeSessionResponse::new()
            .modes(crate::agent::session_registry::mode_state(current_mode))
            .config_options(config_options))
    }

    async fn resume_session_in_runtime(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<SessionActorRef, Error> {
        let session_id = req.session_id.to_string();
        if let Some(session_ref) =
            crate::agent::session_registry::live_session(&self.registry, &session_id).await
        {
            return Ok(session_ref);
        }
        // Keep the owning materializer's single-flight guard through registration
        // and finalization, including concurrent load/resume requests.
        match self
            .session_materializer
            .prepare_resume_session(req, Some(&self.registry))
            .await?
        {
            PreparedSessionResult::Prepared(prepared) => {
                let session_ref = crate::agent::session_registry::register_prepared_session(
                    &self.registry,
                    &prepared,
                )
                .await;
                self.session_materializer
                    .finalize_session(&prepared)
                    .await?;
                Ok(session_ref)
            }
            PreparedSessionResult::AlreadyRegistered(session_ref) => Ok(session_ref),
        }
    }

    async fn run_session_end_hook(&self, session_id: &str, reason: &str) -> Result<(), Error> {
        let permission_mode = {
            let mode = *self
                .default_mode
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            crate::hooks::permission_mode_label(mode).to_string()
        };
        let result = self
            .config
            .hooks
            .run_session_end(crate::hooks::SessionEndRequest {
                session_id: session_id.to_string(),
                cwd: None,
                model: String::new(),
                permission_mode,
                reason: reason.to_string(),
            })
            .await
            .map_err(|error| Error::internal_error().data(error.to_string()))?;
        for notice in result.notices {
            self.config.emit_event(
                session_id,
                crate::events::AgentEventKind::HookNotice {
                    event_name: notice.event_name,
                    message: notice.message,
                    is_error: notice.is_error,
                },
            );
        }
        Ok(())
    }

    pub(super) async fn handle_close_session(
        &self,
        req: CloseSessionRequest,
    ) -> Result<CloseSessionResponse, Error> {
        let session_id = req.session_id.to_string();
        let result = self.stop_session(&session_id).await;
        if let Err(error) = self.run_session_end_hook(&session_id, "close").await {
            tracing::warn!(session_id, %error, "session_end hook failed during close");
        }
        self.clear_delegate_model_overrides(&session_id).await;
        result?;
        Ok(CloseSessionResponse::new())
    }

    pub(super) async fn handle_delete_session(
        &self,
        req: DeleteSessionRequest,
    ) -> Result<DeleteSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        // Closing is best-effort: deleting persisted history is the primary intent.
        // Route through the session binding so delegated runtimes are released too.
        let _ = self.stop_session(&session_id).await;
        if let Err(error) = self.run_session_end_hook(&session_id, "delete").await {
            tracing::warn!(session_id, %error, "session_end hook failed during delete");
        }

        let store = self.config.provider.history_store();
        let exists = store
            .get_session(&session_id)
            .await
            .map_err(|e| Error::internal_error().data(serde_json::json!({"error": e.to_string()})))?
            .is_some();
        if !exists {
            self.clear_delegate_model_overrides(&session_id).await;
            if let Some(profiles) = self.profiles() {
                profiles.forget_session_binding(&session_id).await;
            }
            return Ok(DeleteSessionResponse::new());
        }

        store.delete_session(&session_id).await.map_err(|e| {
            Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
        })?;
        self.config.remove_run_lease_file(&session_id);
        if store.supports_session_pins() {
            crate::control::notifications::emit_session_pins_changed_notification(
                self,
                &session_id,
            )
            .await;
        }
        self.clear_delegate_model_overrides(&session_id).await;
        if let Some(profiles) = self.profiles() {
            profiles.forget_session_binding(&session_id).await;
        }

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
