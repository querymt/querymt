use super::utils::format_prefixed_error_chain;
use super::*;

impl LocalAgentHandle {
    /// Construct a `LocalAgentHandle` from a shared `AgentConfig`.
    ///
    /// This is the canonical way to create a `LocalAgentHandle` after building
    /// an `AgentConfig` via `AgentConfigBuilder::build()`.
    pub fn from_config(config: Arc<AgentConfig>) -> Self {
        #[cfg(feature = "remote")]
        let (remote_disconnect_tx, mut remote_disconnect_rx) =
            tokio::sync::mpsc::unbounded_channel();
        let mut registry_inner = SessionRegistry::new(config.clone());
        #[cfg(feature = "remote")]
        registry_inner.set_remote_disconnect_tx(remote_disconnect_tx);
        let registry = Arc::new(Mutex::new(registry_inner));
        #[cfg(feature = "remote")]
        {
            let registry = registry.clone();
            tokio::spawn(async move {
                while let Some(disconnect) = remote_disconnect_rx.recv().await {
                    let mut registry = registry.lock().await;
                    let _ = registry
                        .detach_remote_session_if_relay_matches(
                            &disconnect.session_id,
                            disconnect.relay_actor_id,
                        )
                        .await;
                }
            });
        }
        let session_materializer = Arc::new(SessionMaterializer::new(config.clone()));
        let model_inventory = crate::model_inventory::ModelInventory::new(config.clone());
        let oauth_service = crate::auth::service::OAuthService::new(
            config.clone(),
            model_inventory.clone(),
            std::sync::Arc::new(Mutex::new(std::collections::HashMap::new())),
            std::sync::Arc::new(Mutex::new(None)),
        );
        Self {
            config,
            registry,
            session_materializer,
            client_state: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            default_mode: StdMutex::new(crate::agent::core::AgentMode::Build),
            default_reasoning_effort: ArcSwap::from_pointee(None),
            #[cfg(feature = "remote")]
            mesh: StdMutex::new(None),
            #[cfg(feature = "remote")]
            local_mesh_actor_refs: OnceCell::new(),
            #[cfg(feature = "remote")]
            local_mesh_node_name: StdMutex::new(None),
            #[cfg(feature = "remote")]
            published_mesh_scopes: StdMutex::new(std::collections::HashSet::new()),
            #[cfg(feature = "remote")]
            remote_node_cache: Arc::new(RemoteNodeMetadataCache::new()),
            model_inventory,
            oauth_service,
            profiles: ArcSwap::from_pointee(None),
            scheduler_handle: Arc::new(parking_lot::Mutex::new(None)),
            shutdown_done: AtomicBool::new(false),
        }
    }

    /// Subscribes to agent events via the fanout (live stream).
    pub fn subscribe_events(&self) -> broadcast::Receiver<crate::events::EventEnvelope> {
        self.config.event_sink.fanout().subscribe()
    }

    pub fn set_profiles(&self, profiles: ProfileRuntime) {
        self.profiles.store(Arc::new(Some(profiles)));
    }

    pub fn profiles(&self) -> Option<ProfileRuntime> {
        self.profiles.load_full().as_ref().clone()
    }

    pub(super) async fn session_config_options(
        &self,
        session_id: Option<&str>,
        mode: AgentMode,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<Vec<SessionConfigOption>, Error> {
        let Some(session_id) = session_id else {
            return Ok(crate::agent::session_registry::config_options(
                mode,
                reasoning_effort,
            ));
        };

        let Some(profiles) = self.profiles() else {
            return Ok(crate::agent::session_registry::config_options(
                mode,
                reasoning_effort,
            ));
        };

        let Some(binding) = profiles.session_binding(session_id).await else {
            return Ok(crate::agent::session_registry::config_options(
                mode,
                reasoning_effort,
            ));
        };

        let profile_list = profiles.list_profiles().await.map_err(|err| {
            Error::internal_error().data(serde_json::json!({
                "message": format_prefixed_error_chain("Failed to list profiles", &err),
            }))
        })?;

        Ok(
            crate::agent::session_registry::config_options_with_profiles(
                mode,
                reasoning_effort,
                Some(binding.profile_id.as_str()),
                &profile_list,
            ),
        )
    }

    /// Acquire the registry lock with tracing for wait and hold durations.
    ///
    /// Returns a guard after recording `registry.lock.wait_ms` for lock acquisition.
    ///
    /// Hold duration is not instrumented here because the plain mutex guard does not
    /// provide a drop hook for recording `registry.lock.hold_ms`.
    pub async fn registry_lock(&self) -> tokio::sync::MutexGuard<'_, SessionRegistry> {
        let start = std::time::Instant::now();
        let guard = self.registry.lock().await;
        let wait_ms = start.elapsed().as_millis() as u64;

        if wait_ms > 10 {
            tracing::warn!(
                wait_ms,
                "Registry lock wait exceeded 10ms - possible contention"
            );
        }

        // Note: We can't easily instrument hold duration with a simple guard
        // because the guard needs to record the time on drop.
        // For now, we rely on callers to use tracing spans if needed.
        // The key insight is that with the 3-phase pattern, hold duration
        // should be microseconds for register_prepared_session().
        guard
    }

    /// Access the agent registry.
    pub fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.config.agent_registry.clone()
    }

    /// Access the tool registry for built-in tool execution.
    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.config.tool_registry_arc()
    }

    /// Access the pending elicitations map for resolving tool and MCP server elicitation requests.
    pub fn pending_elicitations(&self) -> crate::elicitation::PendingElicitationMap {
        self.config.pending_elicitations()
    }

    /// Access the workspace manager actor ref.
    pub fn workspace_manager_actor(&self) -> ActorRef<WorkspaceIndexManagerActor> {
        self.config.workspace_manager_actor()
    }

    /// Sets the client bridge for ACP stdio communication.
    ///
    /// Also propagates the bridge to the session registry so that newly
    /// created sessions receive it via `SetBridge`.
    pub async fn set_bridge(&self, bridge: ClientBridgeSender) {
        if let Ok(mut handle) = self.bridge.lock() {
            *handle = Some(bridge.clone());
        }
        self.registry.lock().await.set_bridge(bridge);
    }

    /// Emits an event for external observers.
    ///
    /// This is a detached fire-and-forget API.
    /// FIXME: Prefer an awaited emit path for critical flows.
    pub fn emit_event(&self, session_id: &str, kind: crate::events::AgentEventKind) {
        self.config.emit_event(session_id, kind);
    }

    /// Start the scheduler actor if a schedule repository is configured.
    ///
    /// This should be called after construction, during agent startup.
    /// Returns `true` if the scheduler was started (lease acquired).
    /// Returns `false` if no schedule repository is configured or the lease
    /// was not acquired (another scheduler is active).
    pub async fn start_scheduler(&self) -> bool {
        let schedule_repo = match &self.config.schedule_repository {
            Some(repo) => repo.clone(),
            None => {
                log::debug!(
                    "LocalAgentHandle: no schedule repository configured, skipping scheduler"
                );
                return false;
            }
        };

        let handle = crate::scheduler::SchedulerActor::spawn(
            schedule_repo,
            self.config.provider.history_store(),
            self.registry.clone(),
            self.config.clone(),
            crate::scheduler::SchedulerConfig::default(),
        )
        .await;

        match handle {
            Some(h) => {
                *self.scheduler_handle.lock() = Some(h);
                log::info!("LocalAgentHandle: scheduler started");
                true
            }
            None => {
                log::info!("LocalAgentHandle: scheduler not started (lease not acquired)");
                false
            }
        }
    }

    /// Get a reference to the scheduler handle, if the scheduler is running.
    pub fn scheduler(&self) -> Option<crate::scheduler::SchedulerHandle> {
        self.scheduler_handle.lock().clone()
    }

    pub(super) fn clear_scheduler_handle(&self) {
        *self.scheduler_handle.lock() = None;
    }

    pub(super) fn scheduler_unavailable_error() -> agent_client_protocol::Error {
        agent_client_protocol::Error::internal_error().data("Scheduler unavailable".to_string())
    }

    pub(super) fn is_actor_not_running_error(error_message: &str) -> bool {
        error_message.contains("actor not running")
    }

    pub(super) async fn get_or_start_scheduler(&self) -> Option<crate::scheduler::SchedulerHandle> {
        if let Some(scheduler) = self.scheduler() {
            return Some(scheduler);
        }

        if self.start_scheduler().await {
            return self.scheduler();
        }

        None
    }

    /// Create a new session with already-connected MCP peers.
    ///
    /// This is used by mobile FFI clients that manage MCP transport lifetimes
    /// externally (e.g. pipe transports) and want those tools available in
    /// each newly created session.
    ///
    /// Uses the 3-phase materialization pattern:
    /// Create a new session.
    ///
    /// Uses the 3-phase materialization pattern:
    /// 1. Prepare (NO lock): DB creation, MCP init, actor spawn
    /// 2. Register (lock held): Insert into in-memory maps (microseconds)
    /// 3. Finalize (NO lock): DHT registration, event emission
    pub async fn new_session(
        &self,
        req: NewSessionRequest,
    ) -> std::result::Result<NewSessionResponse, Error> {
        // Auth check stays on LocalAgentHandle (connection-level concern)
        if let Ok(state) = self.client_state.lock()
            && let Some(state) = state.as_ref()
        {
            let auth_required = !self.config.auth_methods.is_empty();

            if auth_required && !state.authenticated {
                return Err(Error::auth_required());
            }
        }

        if let Some(profiles) = self.profiles() {
            let profile_id = match requested_profile_id_from_meta(req.meta.as_ref()) {
                Some(profile_id) => profile_id.to_string(),
                None => profiles.active_profile_id().await,
            };
            let runtime = profiles.runtime_for_profile(&profile_id).await.map_err(|err| {
                Error::invalid_params().data(serde_json::json!({
                    "message": format_prefixed_error_chain(&format!("Failed to load profile '{profile_id}'"), &err),
                    "profileId": profile_id,
                }))
            })?;
            let profile_handle = runtime.agent().handle();
            let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
            if let Some(bridge) = bridge {
                profile_handle.set_bridge(bridge).await;
            }
            let prepared = profile_handle
                .session_materializer
                .prepare_new_session(req)
                .await?;
            let session_id = prepared.session_id.clone();
            let session_ref = {
                let mut registry = profile_handle.registry_lock().await;
                registry.register_prepared_session(&prepared).await
            };
            let bridge = profile_handle
                .bridge
                .lock()
                .ok()
                .and_then(|guard| guard.clone());
            profile_handle
                .session_materializer
                .finalize_session(&prepared, bridge)
                .await?;
            profiles
                .bind_session_to_runtime(session_id.clone(), &runtime)
                .await
                .map_err(|err| {
                    Error::internal_error().data(serde_json::json!({
                        "message": format_prefixed_error_chain("Failed to bind session to profile", &err),
                        "profileId": profile_id,
                        "sessionId": session_id,
                    }))
                })?;

            let current_mode = session_ref.get_mode().await.map_err(Error::from)?;
            let profile_list = profiles.list_profiles().await.map_err(|err| {
                Error::internal_error().data(serde_json::json!({
                    "message": format_prefixed_error_chain("Failed to list profiles", &err),
                }))
            })?;
            let config_options = crate::agent::session_registry::config_options_with_profiles(
                current_mode,
                **profile_handle.config.default_reasoning_effort.load(),
                Some(profile_id.as_str()),
                &profile_list,
            );
            let mut meta = serde_json::Map::new();
            meta.insert(
                "querymt".to_string(),
                serde_json::json!({
                    "profile_id": profile_id,
                    "profile_source": "querymt_profile_manager",
                }),
            );
            return Ok(NewSessionResponse::new(session_id)
                .modes(crate::agent::session_registry::mode_state(current_mode))
                .config_options(config_options)
                .meta(meta));
        }

        // Phase 1: Prepare session (heavy work, NO registry lock held)
        let prepared = self.session_materializer.prepare_new_session(req).await?;

        let session_id = prepared.session_id.clone();

        // Phase 2: Register session (fast, registry lock held for microseconds only)
        let session_ref = {
            let mut registry = self.registry_lock().await;
            registry.register_prepared_session(&prepared).await
        };

        // Phase 3: Finalize session (post-registration work, NO registry lock held)
        // Pass the bridge for setup outside the lock
        let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
        self.session_materializer
            .finalize_session(&prepared, bridge)
            .await?;

        // Get current mode for response
        let current_mode = session_ref.get_mode().await.map_err(Error::from)?;

        let config_options = self
            .session_config_options(
                Some(&session_id),
                current_mode,
                **self.config.default_reasoning_effort.load(),
            )
            .await?;

        Ok(NewSessionResponse::new(session_id)
            .modes(crate::agent::session_registry::mode_state(current_mode))
            .config_options(config_options))
    }

    /// Load an existing session. MCP attachments are resolved internally by
    /// the [`SessionMaterializer`] via the runtime attachment source.
    ///
    /// Uses the 3-phase materialization pattern with single-flight protection:
    /// 1. Check registry (lock held briefly): Return existing if already materialized
    /// 2. Prepare (NO lock): DB validation, MCP init, actor spawn
    /// 3. Register (lock held): Insert into in-memory maps (microseconds)
    /// 4. Finalize (NO lock): DHT registration, event emission
    pub async fn load_session(
        &self,
        req: agent_client_protocol::schema::LoadSessionRequest,
    ) -> std::result::Result<agent_client_protocol::schema::LoadSessionResponse, Error> {
        let session_id = req.session_id.to_string();

        if let Some(profiles) = self.profiles()
            && let Some(binding) = profiles.session_binding(&session_id).await
        {
            let runtime = profiles
                .runtime_for_profile(&binding.profile_id)
                .await
                .map_err(|err| {
                    Error::internal_error().data(serde_json::json!({
                        "message": format_prefixed_error_chain(
                            &format!("Failed to load profile '{}'", binding.profile_id),
                            &err,
                        ),
                        "profileId": binding.profile_id,
                        "sessionId": session_id,
                    }))
                })?;
            let profile_handle = runtime.agent().handle();
            let bridge = self.bridge.lock().ok().and_then(|guard| guard.clone());
            if let Some(bridge) = bridge {
                profile_handle.set_bridge(bridge).await;
            }

            let existing_session_ref = {
                let registry = profile_handle.registry_lock().await;
                registry.get(&session_id).cloned()
            };
            let session_ref = if let Some(session_ref) = existing_session_ref {
                session_ref
            } else {
                match profile_handle
                    .session_materializer
                    .prepare_load_session(req, Some(&profile_handle.registry))
                    .await?
                {
                    PreparedSessionResult::Prepared(prepared) => {
                        let session_ref = {
                            let mut registry = profile_handle.registry_lock().await;
                            registry.register_prepared_session(&prepared).await
                        };
                        let bridge = profile_handle
                            .bridge
                            .lock()
                            .ok()
                            .and_then(|guard| guard.clone());
                        profile_handle
                            .session_materializer
                            .finalize_session(&prepared, bridge)
                            .await?;
                        session_ref
                    }
                    PreparedSessionResult::AlreadyRegistered(session_ref) => session_ref,
                }
            };
            let current_mode = session_ref.get_mode().await.map_err(Error::from)?;
            let profile_list = profiles.list_profiles().await.map_err(|err| {
                Error::internal_error().data(serde_json::json!({
                    "message": format_prefixed_error_chain("Failed to list profiles", &err),
                }))
            })?;
            let config_options = crate::agent::session_registry::config_options_with_profiles(
                current_mode,
                **profile_handle.config.default_reasoning_effort.load(),
                Some(binding.profile_id.as_str()),
                &profile_list,
            );
            let mut response = LoadSessionResponse::new()
                .modes(crate::agent::session_registry::mode_state(current_mode))
                .config_options(config_options);
            if let Some(meta) = profile_handle.session_load_snapshot_meta(&session_id).await {
                response = response.meta(meta);
            }
            return Ok(response);
        }

        // Single-flight check: Check if session is already materialized
        // Clone the session_ref out of the registry to avoid holding the lock
        // during the async get_mode() call.
        let existing_session_ref = {
            let registry = self.registry_lock().await;
            registry.get(&session_id).cloned()
        };

        if let Some(session_ref) = existing_session_ref {
            // Registry lock is now dropped, safe to make async actor call
            let current_mode = session_ref.get_mode().await.map_err(Error::from)?;
            let config_options = self
                .session_config_options(
                    Some(&session_id),
                    current_mode,
                    **self.config.default_reasoning_effort.load(),
                )
                .await?;
            let mut response = LoadSessionResponse::new()
                .modes(crate::agent::session_registry::mode_state(current_mode))
                .config_options(config_options);
            if let Some(meta) = self.session_load_snapshot_meta(&session_id).await {
                response = response.meta(meta);
            }
            return Ok(response);
        }

        // Phase 1: Prepare session (heavy work, NO registry lock held)
        // The session_materializer's internal single-flight lock prevents
        // duplicate materialization of the same session ID.
        // Pass registry so it can re-check after acquiring the lock.
        let (prepared, session_ref) = match self
            .session_materializer
            .prepare_load_session(req, Some(&self.registry))
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

        let mut response = LoadSessionResponse::new()
            .modes(crate::agent::session_registry::mode_state(current_mode))
            .config_options(config_options);
        if let Some(meta) = self.session_load_snapshot_meta(&session_id).await {
            response = response.meta(meta);
        }

        Ok(response)
    }

    async fn session_load_snapshot_meta(
        &self,
        session_id: &str,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let view_store = self.config.storage.view_store()?;
        let snapshot = crate::session::load_session_snapshot(self, view_store, session_id)
            .await
            .ok()?;
        let mut meta = serde_json::Map::new();
        meta.insert(
            "querymt/sessionLoadSnapshot.v1".to_string(),
            serde_json::to_value(snapshot).ok()?,
        );
        Some(meta)
    }

    /// Gracefully shutdown the agent and all background tasks.
    pub async fn shutdown(&self) {
        if self.shutdown_done.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("LocalAgentHandle: Starting graceful shutdown");

        // Shutdown the scheduler first
        if let Some(scheduler) = self.scheduler() {
            scheduler.shutdown().await;
        }

        self.config.shutdown().await;

        // Wait briefly for in-flight work to settle, then flush any
        // buffered OTLP spans/logs before the process exits.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        querymt_utils::telemetry::flush_telemetry();

        log::info!("LocalAgentHandle: Shutdown complete");
    }
}

fn requested_profile_id_from_meta(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<&str> {
    let profile_id = meta?.get("querymt")?.get("profile_id")?.as_str()?.trim();
    (!profile_id.is_empty() && profile_id != "default").then_some(profile_id)
}
