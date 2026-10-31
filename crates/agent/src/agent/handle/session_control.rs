use super::utils::format_prefixed_error_chain;
use super::*;

/// Bounds for the runtime-status probe in `unload_idle_in_runtime`, which
/// runs under the registry lock.
const UNLOAD_STATUS_MAILBOX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const UNLOAD_STATUS_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const STOP_LOCAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const STOP_REMOTE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

impl LocalAgentHandle {
    pub(super) async fn bound_session_handle(
        &self,
        session_id: &str,
    ) -> Result<
        Option<(
            Arc<dyn crate::agent::handle::AgentHandle>,
            crate::profiles::SessionProfileBinding,
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

    pub(crate) async fn session_ref_for_agent_session(
        &self,
        session_id: &str,
    ) -> Result<SessionActorRef, Error> {
        if let Some((session_handle, binding)) = self.bound_session_handle(session_id).await? {
            let local_handle = session_handle
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
        if let Some((session_handle, binding)) = self.bound_session_handle(session_id).await? {
            let local_handle = session_handle
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
    pub(crate) async fn terminate_for_delete(
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
    pub(crate) async fn unload_session_if_idle(&self, session_id: &str) -> Result<bool, Error> {
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

    pub(super) async fn stop_session_in_runtime(&self, session_id: &str) -> Result<(), Error> {
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
                let remote_request_timeout = Self::remote_request_timeout();
                let bookmark = self
                    .config
                    .provider
                    .history_store()
                    .list_remote_session_bookmarks()
                    .await
                    .map_err(|e| Error::internal_error().data(e.to_string()))?
                    .into_iter()
                    .find(|b| b.session_id == session_id);

                if let Some(bookmark) = bookmark {
                    if let Some(mesh) = self.mesh() {
                        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
                        let mut provider_host = None;
                        for scope in runtime.active_scopes() {
                            let provider_host_name =
                                crate::agent::remote::scope::scoped_provider_host(
                                    &scope,
                                    &bookmark.node_id,
                                );
                            if let Ok(Some(found)) = mesh
                                .lookup_actor::<querymt_remote::ProviderHostActor>(
                                    &provider_host_name,
                                )
                                .await
                            {
                                provider_host = Some(found);
                                break;
                            }
                        }
                        if let Some(provider_host) = provider_host {
                            let status = querymt_remote::ask_remote_with_timeout(
                                &provider_host,
                                &crate::agent::remote::GetProviderStreamStatus {
                                    session_id: session_id.to_string(),
                                    request_id: None,
                                },
                                remote_request_timeout,
                            )
                            .await
                            .ok()
                            .flatten();
                            if let Some(status) = status {
                                tracing::warn!(
                                    session_id,
                                    request_id = %status.request_id,
                                    phase = ?status.phase,
                                    elapsed_ms = status.elapsed_ms,
                                    idle_ms = status.idle_ms,
                                    chunk_count = status.chunk_count,
                                    receiver_connected = status.receiver_connected,
                                    lease_expires_in_ms = status.lease_expires_in_ms,
                                    provider = %status.provider,
                                    model = %status.model,
                                    last_error = ?status.last_error,
                                    "remote stop found active provider stream; issuing provider-host cancel"
                                );
                                let _ = querymt_remote::ask_remote_with_timeout(
                                    &provider_host,
                                    &crate::agent::remote::CancelProviderStreamRequest {
                                        session_id: session_id.to_string(),
                                        request_id: Some(status.request_id.clone()),
                                        reason: Some("session stop requested".to_string()),
                                    },
                                    remote_request_timeout,
                                )
                                .await;
                            } else {
                                let _ = querymt_remote::ask_remote_with_timeout(
                                    &provider_host,
                                    &crate::agent::remote::CancelProviderStreamRequest {
                                        session_id: session_id.to_string(),
                                        request_id: None,
                                        reason: Some(
                                            "session stop requested without status".to_string(),
                                        ),
                                    },
                                    remote_request_timeout,
                                )
                                .await;
                            }
                        }
                    }

                    let nm_ref = self.find_node_manager(&bookmark.node_id).await?;
                    querymt_remote::ask_remote_with_timeout(
                        &nm_ref,
                        &crate::agent::remote::StopRemoteSessionRuntime {
                            session_id: session_id.to_string(),
                        },
                        remote_request_timeout,
                    )
                    .await
                    .map_err(|e| {
                        Error::from(match querymt_remote::classify_remote_send_error(e) {
                            Ok(failure) => {
                                crate::error::AgentError::from_transport_failure(failure)
                            }
                            Err(handler_error) => {
                                crate::error::AgentError::RemoteActor(handler_error.to_string())
                            }
                        })
                    })?;
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
