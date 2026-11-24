use super::utils::format_prefixed_error_chain;
use super::*;

/// Bounds for the runtime-status probe in `unload_idle_in_runtime`, which
/// runs under the registry lock.
const UNLOAD_STATUS_MAILBOX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const UNLOAD_STATUS_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const STOP_LOCAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const STOP_REMOTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

impl LocalAgentHandle {
    /// In profile mode, give a locally stored session that has no usable profile
    /// binding one (see `ProfileRuntimeManager::adopt_unbound_session`), so history
    /// created before profiles existed, or by a config-file run, stays openable.
    /// Unknown session ids are left alone and fail later as before.
    pub(super) async fn adopt_unbound_session_binding(
        &self,
        session_id: &str,
    ) -> Result<(), Error> {
        let Some(profiles) = self.profiles() else {
            return Ok(());
        };
        if profiles.session_binding(session_id).await.is_some() {
            return Ok(());
        }
        let exists = self
            .config
            .provider
            .history_store()
            .get_session(session_id)
            .await
            .map_err(|err| Error::internal_error().data(err.to_string()))?
            .is_some();
        if !exists {
            return Ok(());
        }
        profiles
            .adopt_unbound_session(session_id)
            .await
            .map_err(|err| {
                Error::internal_error().data(serde_json::json!({
                    "message": format_prefixed_error_chain(
                        "Failed to bind session to a profile",
                        &err,
                    ),
                    "sessionId": session_id,
                }))
            })?;
        Ok(())
    }

    pub(super) async fn bound_session_handle(
        &self,
        session_id: &str,
    ) -> Result<
        Option<(
            Arc<LocalAgentHandle>,
            crate::profile_host::SessionProfileBinding,
        )>,
        Error,
    > {
        let Some(profiles) = self.profiles() else {
            return Ok(None);
        };
        let Some(binding) = profiles.session_binding(session_id).await else {
            return Ok(None);
        };
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
        let session_handle = runtime
            .session_handle(binding.agent_id.as_deref())
            .ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "session is bound to an unknown delegate",
                    "sessionId": session_id,
                    "profileId": binding.profile_id,
                    "agentId": binding.agent_id,
                }))
            })?;
        Ok(Some((session_handle, binding)))
    }

    /// Live actor for a session, wherever it is registered: this handle or an
    /// already built profile runtime or delegate. `None` when no actor is
    /// materialized. Unlike [`Self::bound_session_handle`], never builds a
    /// runtime or delegate.
    pub(crate) async fn owned_session_ref(&self, session_id: &str) -> Option<SessionActorRef> {
        let own = self.registry.lock().await.get(session_id).cloned();
        if own.is_some() {
            return own;
        }
        // Observe only: neither the binding lookup nor delegate resolution may
        // run, both build runtimes. An unbuilt runtime holds no live actor.
        for runtime in self.profiles()?.materialized_runtimes().await {
            let handle = runtime.handle();
            let delegates = handle.local_members();
            for handle in std::iter::once(handle).chain(delegates) {
                let session_ref = handle.registry.lock().await.get(session_id).cloned();
                if session_ref.is_some() {
                    return session_ref;
                }
            }
        }
        None
    }

    #[doc(hidden)]
    pub async fn session_ref_for_agent_session(
        &self,
        session_id: &str,
    ) -> Result<SessionActorRef, Error> {
        if let Some((session_handle, binding)) = self.bound_session_handle(session_id).await? {
            let local_handle = &*session_handle;
            let registry = local_handle.registry.lock().await;
            return registry.get(session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session for bound runtime",
                    "sessionId": session_id,
                    "profileId": binding.profile_id,
                    "agentId": binding.agent_id,
                }))
            });
        }

        let registry = self.registry.lock().await;
        registry.get(session_id).cloned().ok_or_else(|| {
            Error::invalid_params().data(serde_json::json!({
                "message": "unknown session",
                "sessionId": session_id,
            }))
        })
    }

    pub async fn stop_session(&self, session_id: &str) -> Result<(), Error> {
        if let Some((session_handle, _binding)) = self.bound_session_handle(session_id).await? {
            let local_handle = &*session_handle;
            return local_handle.stop_session_in_runtime(session_id).await;
        }

        self.stop_session_in_runtime(session_id).await
    }

    /// Tear down `session_id`'s local actor for deletion: no graceful-stop
    /// wait, no stop events. Cancelling and shutting the actor down is enough
    /// because nothing will read the run's outcome once the session is gone.
    /// Routed to the bound runtime like [`Self::stop_session`]. Remote-hosted
    /// sessions keep the full stop protocol; unloaded sessions are a no-op.
    ///
    /// Returns the per-session materialization guard of the runtime that owns
    /// the session. The caller must keep it until the session row is deleted:
    /// a concurrent load/resume of this session then waits, and afterwards
    /// finds no row instead of registering an actor for a deleted session.
    /// The guard is per session, so creating sessions and materializing other
    /// sessions are never blocked. Teardown is best-effort (failures are
    /// logged); the guard is returned regardless.
    #[doc(hidden)]
    pub async fn terminate_for_delete(
        &self,
        session_id: &str,
    ) -> crate::agent::session_materializer::SessionSingleFlightGuard {
        // Resolve the owning runtime before locking: this may build a profile
        // runtime, which must not happen while loads of this session wait.
        let bound = match self.bound_session_handle(session_id).await {
            Ok(bound) => bound,
            Err(error) => {
                tracing::warn!(session_id, %error, "failed to resolve session runtime for delete");
                None
            }
        };
        let bound_handle = bound
            .as_ref()
            .and_then(|(handle, _)| handle.as_any().downcast_ref::<LocalAgentHandle>());
        let runtime: &LocalAgentHandle = bound_handle.unwrap_or(self);

        let guard = runtime.session_materializer.lock_session(session_id).await;
        if let Err(error) = Box::pin(runtime.terminate_session_in_runtime(session_id)).await {
            tracing::warn!(session_id, %error, "failed to terminate session actor for delete");
        }
        guard
    }

    /// Drop the session's todo list. A profile-bound session runs with its
    /// runtime's own `AgentConfig`, so its list lives in that runtime's store
    /// rather than this handle's; clear both. Call before the profile binding
    /// is forgotten.
    pub(crate) async fn discard_session_todos(&self, session_id: &str) {
        self.config.todo_store.remove(session_id);
        match self.bound_session_handle(session_id).await {
            Ok(Some((handle, _))) => {
                if let Some(local) = handle.as_any().downcast_ref::<LocalAgentHandle>() {
                    local.config.todo_store.remove(session_id);
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(session_id, %error, "failed to resolve session runtime for todo cleanup");
            }
        }
    }

    async fn terminate_session_in_runtime(&self, session_id: &str) -> Result<(), Error> {
        let removed = {
            let mut registry = self.registry.lock().await;
            match registry.get(session_id) {
                None => return Ok(()),
                Some(session) if session.is_remote() => None,
                Some(_) => registry.remove(session_id),
            }
        };
        let Some(session_ref) = removed else {
            return self.stop_session_in_runtime(session_id).await;
        };
        if let Err(err) = session_ref.shutdown().await {
            tracing::warn!(session_id, error = %err, "failed to shut down deleted session actor");
        }
        Ok(())
    }

    /// Unload `session_id`'s actor when it is loaded and idle, without
    /// cancelling anything. Returns `Ok(true)` when the session is busy (a
    /// running turn, queued or steered input, or a remote host) and was left
    /// untouched, `Ok(false)` when it is now unloaded or never was loaded.
    #[doc(hidden)]
    pub async fn unload_session_if_idle(&self, session_id: &str) -> Result<bool, Error> {
        if let Some((session_handle, _)) = self.bound_session_handle(session_id).await?
            && let Some(local) = session_handle.as_any().downcast_ref::<LocalAgentHandle>()
        {
            return Box::pin(local.unload_idle_in_runtime(session_id)).await;
        }
        self.unload_idle_in_runtime(session_id).await
    }

    async fn unload_idle_in_runtime(&self, session_id: &str) -> Result<bool, Error> {
        use crate::agent::messages::SessionRuntimePhase;

        // The idle check and the removal happen under one registry lock, so a
        // prompt cannot look the actor up between them: it either runs first
        // (the session is then busy) or finds the session unloaded.
        let removed = {
            let mut registry = self.registry.lock().await;
            let Some(session) = registry.get(session_id).cloned() else {
                return Ok(false);
            };
            if session.is_remote() {
                return Ok(true);
            }
            // Bounded: a stalled actor must not hold the registry lock
            // forever. A timeout is treated as busy and the session is left
            // loaded.
            let status = match session
                .get_runtime_status_with_timeout(
                    UNLOAD_STATUS_MAILBOX_TIMEOUT,
                    UNLOAD_STATUS_REPLY_TIMEOUT,
                )
                .await
            {
                Ok(status) => status,
                Err(crate::error::AgentError::SessionTimeout { .. }) => return Ok(true),
                Err(err) => return Err(Error::from(err)),
            };
            if status.phase != SessionRuntimePhase::Idle
                || status.queued_input_count > 0
                || status.pending_steering_count > 0
            {
                return Ok(true);
            }
            registry.remove(session_id)
        };
        if let Some(session) = removed
            && let Err(err) = session.shutdown().await
        {
            tracing::warn!(session_id, error = %err, "failed to shut down idle session actor");
        }
        Ok(false)
    }

    #[doc(hidden)]
    pub async fn stop_session_in_runtime(&self, session_id: &str) -> Result<(), Error> {
        use crate::agent::messages::{SessionRuntimePhase, SessionRuntimeStatus};

        const STOP_ESCALATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned()
        };

        let Some(session_ref) = session_ref else {
            // Not loaded here, so no run of this process exists. An unmatched
            // run_started can only be an orphan; close it (local sessions only,
            // reconciliation skips remote bookmarks).
            let closed =
                crate::session::run_reconcile::reconcile_orphaned_run(&self.config, session_id)
                    .await;
            if closed.is_empty() {
                log::warn!(
                    "Stop requested for session {} but not found in registry",
                    session_id
                );
            }
            return Ok(());
        };

        self.config
            .emit_event(session_id, AgentEventKind::SessionStopRequested);
        let _ = session_ref.cancel().await;

        // Give the cancelled run up to STOP_ESCALATION_TIMEOUT to wind down,
        // returning as soon as the session is idle. The mailbox is ordered, so
        // the first status already reflects the cancel above.
        let poll_interval = if session_ref.is_remote() {
            STOP_REMOTE_POLL_INTERVAL
        } else {
            STOP_LOCAL_POLL_INTERVAL
        };
        let deadline = tokio::time::Instant::now() + STOP_ESCALATION_TIMEOUT;
        loop {
            let idle = matches!(
                session_ref.get_runtime_status().await,
                Ok(status) if status.phase == SessionRuntimePhase::Idle
            );
            let now = tokio::time::Instant::now();
            if idle || now >= deadline {
                break;
            }
            tokio::time::sleep(poll_interval.min(deadline - now)).await;
        }

        let status = session_ref
            .get_runtime_status()
            .await
            .unwrap_or(SessionRuntimeStatus::Running);
        let stopped_gracefully = status.phase == SessionRuntimePhase::Idle
            || (status.phase == SessionRuntimePhase::CancelRequested && !session_ref.is_remote());
        if stopped_gracefully {
            tracing::debug!(
                session_id,
                phase = ?status.phase,
                "session stop completed gracefully; releasing runtime resources",
            );
        } else {
            if status.phase == SessionRuntimePhase::CancelRequested {
                tracing::warn!(
                    "Session {} stop: still CancelRequested after {:?}; escalating to force-stop",
                    session_id,
                    STOP_ESCALATION_TIMEOUT
                );
            }

            self.config.emit_event(
                session_id,
                AgentEventKind::SessionForceStopped {
                    escalated_after_ms: STOP_ESCALATION_TIMEOUT.as_millis() as u64,
                    reason: "graceful cancellation timeout elapsed".to_string(),
                },
            );
        }

        if session_ref.is_remote() {
            #[cfg(feature = "remote")]
            {
                let bookmark = self
                    .config
                    .provider
                    .history_store()
                    .list_remote_session_bookmarks()
                    .await
                    .map_err(|e| Error::internal_error().data(e.to_string()))?
                    .into_iter()
                    .find(|b| b.session_id == session_id);

                // Stopping the owner's runtime cancels its run; the owner's
                // provider client then cancels the stream on the provider host
                // when the run drops it. A viewer cannot control those streams,
                // since provider hosts only accept stream controls from the
                // peer that opened the stream.
                if let Some(bookmark) = bookmark {
                    let node_manager = self.find_node_manager(&bookmark.node_id).await?;
                    self.ask_node_manager::<_, Error>(
                        &node_manager,
                        &crate::agent::remote::StopRemoteSessionRuntime {
                            session_id: session_id.to_string(),
                        },
                    )
                    .await?;
                }

                self.detach_remote_session_attachment(session_id, true)
                    .await;
            }
        } else {
            let removed = {
                let mut registry = self.registry.lock().await;
                registry.remove(session_id)
            };
            if let Some(session_ref) = removed
                && let Err(err) = session_ref.shutdown().await
            {
                tracing::warn!(session_id, error = %err, "failed to shut down session actor");
            }
        }

        Ok(())
    }
}
