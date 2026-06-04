use super::*;

impl LocalAgentHandle {
    pub(super) async fn session_ref_for_agent_session(
        &self,
        session_id: &str,
    ) -> Result<SessionActorRef, Error> {
        let registry = self.registry.lock().await;
        registry.get(session_id).cloned().ok_or_else(|| {
            Error::invalid_params().data(serde_json::json!({
                "message": "unknown session",
                "sessionId": session_id,
            }))
        })
    }

    pub async fn stop_session(&self, session_id: &str) -> Result<(), Error> {
        use crate::agent::messages::SessionRuntimeStatus;

        const STOP_ESCALATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned()
        };

        let Some(session_ref) = session_ref else {
            log::warn!(
                "Stop requested for session {} but not found in registry",
                session_id
            );
            return Ok(());
        };

        self.config
            .emit_event(session_id, AgentEventKind::SessionStopRequested);
        let _ = session_ref.cancel().await;

        tokio::time::sleep(STOP_ESCALATION_TIMEOUT).await;

        let status = session_ref
            .get_runtime_status()
            .await
            .unwrap_or(SessionRuntimeStatus::Running);
        if status == SessionRuntimeStatus::Idle {
            tracing::debug!(
                "Session {} stop: status=Idle, graceful shutdown — returning without force-stop",
                session_id,
            );
            return Ok(());
        }
        // For remote sessions, CancelRequested doesn't mean the prompt is done —
        // the provider stream might still be active on the remote node.
        if status == SessionRuntimeStatus::CancelRequested && !session_ref.is_remote() {
            tracing::debug!(
                "Session {} stop: status=CancelRequested (local), graceful shutdown — returning without force-stop",
                session_id,
            );
            return Ok(());
        }

        if matches!(status, SessionRuntimeStatus::CancelRequested) {
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
                        Error::from(crate::error::AgentError::RemoteActor(e.to_string()))
                    })?;
                }

                let mut registry = self.registry.lock().await;
                registry
                    .detach_remote_session_preserve_bookmark(session_id)
                    .await;
            }
        } else {
            let removed = {
                let mut registry = self.registry.lock().await;
                registry.remove(session_id)
            };
            if let Some(session_ref) = removed {
                let _ = session_ref.shutdown().await;
            }
        }

        Ok(())
    }
}
