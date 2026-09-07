use super::*;

impl LocalAgentHandle {
    #[cfg(feature = "remote")]
    fn map_remote_node_manager_error(
        error: kameo::error::RemoteSendError<crate::error::AgentError>,
    ) -> agent_client_protocol::Error {
        use crate::error::AgentError;

        match querymt_remote::classify_remote_send_error(error) {
            Ok(failure) => {
                agent_client_protocol::Error::from(AgentError::from_transport_failure(failure))
            }
            Err(handler_error) => agent_client_protocol::Error::from(handler_error),
        }
    }

    /// Typed failure for the attachment health-check timeout: ReplyTimeout with
    /// Unknown delivery, so recovery decisions can consult `transport_failure()`
    /// instead of parsing error strings.
    #[cfg(feature = "remote")]
    pub(crate) fn remote_health_check_timeout_error(
        health_timeout: std::time::Duration,
    ) -> crate::error::AgentError {
        crate::error::AgentError::RemoteTransport(querymt_remote::RemoteTransportFailure::new(
            querymt_remote::RemoteTransportFailureKind::ReplyTimeout,
            querymt_remote::DeliveryCertainty::Unknown,
            format!(
                "remote attachment health check timed out after {}ms",
                health_timeout.as_millis()
            ),
        ))
    }

    /// Find a `RemoteNodeManager` by its stable node id (PeerId string).
    ///
    /// ## Fast path
    ///
    /// If `node_id` parses as a `PeerId`, uses the mesh route table to pick the
    /// best-known scope for that peer first (LAN beats iroh when both exist),
    /// then performs a direct per-peer DHT lookup under that scope. This keeps
    /// routine targeted actions on the same path as current reachability.
    ///
    /// ## Fallback scan
    ///
    /// If the direct lookup misses (e.g. the remote node is running an older
    /// version that only registers under the global `"node_manager"` name),
    /// falls back to iterating all `RemoteNodeManager` actors via
    /// `lookup_all_actors` and comparing `GetNodeInfo.node_id`.  Unlike
    /// `list_remote_nodes`, this scan deliberately **skips the `is_peer_alive`
    /// filter**: the user has explicitly requested this node, so we attempt
    /// `GetNodeInfo` contact (3 s timeout) before giving up rather than
    /// silently discarding the candidate.
    #[cfg(feature = "remote")]
    pub async fn find_node_manager(
        &self,
        node_id: &str,
    ) -> Result<
        kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        agent_client_protocol::Error,
    > {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::{StreamExt, stream::FuturesUnordered};
        use querymt_remote::ask_remote_with_timeout;

        use crate::error::AgentError;
        let mesh = self
            .mesh()
            .ok_or_else(|| agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped))?;

        self.ensure_remote_node_cache_invalidation_task(&mesh);

        // ── Fast path: direct per-peer DHT lookup ────────────────────────────
        //
        // Remote nodes register under both the global "node_manager" name (for
        // mesh-wide discovery) and a per-peer "node_manager::peer::{peer_id}"
        // name (for this O(1) lookup). The per-peer lookup bypasses the
        // is_peer_alive gate that guards the fallback scan, so it works even
        // when mDNS has temporarily expired the peer's heartbeat.
        let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
        let parsed_peer_id = node_id.parse::<libp2p::PeerId>().ok();
        let mut direct_scopes = Vec::new();
        if let Some(peer_id) = parsed_peer_id
            && let Some(best_route) = mesh.best_route_for_peer(&peer_id)
        {
            direct_scopes.push(best_route.scope);
        }
        if direct_scopes.is_empty() {
            direct_scopes.push(crate::agent::remote::scope::MeshScopeId::lan_default());
        }
        for scope in runtime.active_scopes() {
            if !direct_scopes.contains(&scope) {
                direct_scopes.push(scope);
            }
        }

        for scope in &direct_scopes {
            let direct_dht_name =
                crate::agent::remote::scope::scoped_node_manager_for_peer(scope, &node_id);
            match runtime
                .lookup_actor_no_retry::<RemoteNodeManager>(direct_dht_name.clone())
                .await
            {
                Ok(Some(node_manager_ref)) => {
                    log::debug!(
                        "find_node_manager: fast-path DHT hit for '{}'",
                        direct_dht_name
                    );
                    return Ok(node_manager_ref);
                }
                Ok(None) => {
                    log::debug!(
                        "find_node_manager: no direct DHT entry for '{}', trying next scope",
                        direct_dht_name
                    );
                }
                Err(e) => {
                    log::debug!(
                        "find_node_manager: direct DHT lookup error for '{}': {}, trying next scope",
                        direct_dht_name,
                        e
                    );
                }
            }
        }

        // ── Fallback scan: iterate all registered RemoteNodeManagers ─────────
        //
        // NOTE: unlike list_remote_nodes, we do NOT filter by is_peer_alive
        // here. The user explicitly chose this node, so we attempt GetNodeInfo
        // contact before giving up. The 3-second timeout on GetNodeInfo is the
        // real liveness check for a targeted user action.
        let local_peer_id = *mesh.peer_id();
        let timeout = Self::remote_node_info_timeout();
        let concurrency = Self::remote_node_lookup_parallelism();
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut lookups = FuturesUnordered::new();

        for scope in runtime.active_scopes() {
            let mut stream = runtime.lookup_all_actors::<RemoteNodeManager>(
                crate::agent::remote::scope::scoped_node_manager(&scope),
            );
            while let Some(result) = stream.next().await {
                match result {
                    Ok(node_manager_ref) => {
                        let peer_id = node_manager_ref.id().peer_id().copied();
                        if peer_id == Some(local_peer_id) {
                            continue;
                        }
                        // No is_peer_alive check here — we contact the peer
                        // directly and let the GetNodeInfo timeout decide.

                        let cache_key =
                            Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                        if let Some(info) = self.get_cached_remote_node(&cache_key) {
                            if info.node_id.to_string() == node_id {
                                return Ok(node_manager_ref);
                            }
                            continue;
                        }

                        let semaphore = Arc::clone(&semaphore);
                        lookups.push(async move {
                            let permit = semaphore.acquire_owned().await.ok();
                            let res =
                                ask_remote_with_timeout(&node_manager_ref, &GetNodeInfo, timeout)
                                    .await;
                            drop(permit);
                            (node_manager_ref, cache_key, peer_id, res)
                        });
                    }
                    Err(e) => {
                        log::warn!("find_node_manager: lookup error: {}", e);
                    }
                }
            }
        }

        while let Some((node_manager_ref, cache_key, peer_id, result)) = lookups.next().await {
            match result {
                Ok(info) => {
                    self.insert_cached_remote_node(cache_key, info.clone());
                    if info.node_id.to_string() == node_id {
                        return Ok(node_manager_ref);
                    }
                }
                Err(kameo::error::RemoteSendError::ReplyTimeout) => {
                    log::warn!(
                        "find_node_manager: GetNodeInfo timed out for peer {:?}",
                        peer_id
                    );
                }
                Err(e) => {
                    log::warn!("find_node_manager: GetNodeInfo failed: {}", e);
                }
            }
        }

        Err(agent_client_protocol::Error::from(
            AgentError::RemoteSessionNotFound {
                details: format!(
                    "Remote node id '{}' not found in the mesh. \
                     The node may have gone offline or mDNS discovery may not have \
                     completed yet. Available nodes can be listed via list_remote_nodes.",
                    node_id
                ),
            },
        ))
    }

    /// List sessions on a specific remote node.
    ///
    /// Sends `ListRemoteSessions` to the `RemoteNodeManager` registered under
    /// `node_manager_name` in the Kademlia DHT.
    ///
    /// Requires a bootstrapped swarm (Phase 6). Returns an error if the node
    /// is not reachable or has no registered `RemoteNodeManager`.
    #[cfg(feature = "remote")]
    pub async fn list_remote_sessions(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Result<
        crate::agent::remote::node_manager::ListRemoteSessionsResponse,
        agent_client_protocol::Error,
    > {
        use crate::agent::remote::ListRemoteSessions;
        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &ListRemoteSessions { offset, limit },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    /// Create a session on a remote node and return the owning node's live session ref.
    ///
    /// Callers can immediately finalize local attachment from the returned capability
    /// while DHT registration continues as background discoverability for reconnects.
    #[cfg(feature = "remote")]
    pub async fn create_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        cwd: Option<String>,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::CreateRemoteSession;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &CreateRemoteSession { cwd },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    /// Fork a session on a remote node and return the forked child's live session ref.
    #[cfg(feature = "remote")]
    pub async fn fork_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        source_session_id: String,
        message_id: String,
    ) -> Result<crate::agent::remote::ForkRemoteSessionResponse, agent_client_protocol::Error> {
        use crate::agent::remote::ForkRemoteSession;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &ForkRemoteSession {
                source_session_id,
                message_id,
            },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    /// Attach an existing remote session (already has a `RemoteActorRef`) to
    /// the local registry.
    ///
    /// This is the lower-level entry point used when the caller already has a
    /// `RemoteActorRef<SessionActor>` (e.g., obtained via swarm lookup after
    /// Phase 6 bootstrap).
    #[cfg(feature = "remote")]
    pub async fn attach_remote_session(
        &self,
        session_id: String,
        remote_ref: kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        peer_label: String,
        preferred_scope: Option<crate::agent::remote::scope::MeshScopeId>,
        remote_node_id: Option<String>,
    ) -> Result<crate::agent::remote::SessionActorRef, crate::error::AgentError> {
        let mesh = self.mesh();
        let bookmark_peer_label = peer_label.clone();
        let bookmark_node_id = remote_node_id.clone();
        let backfill_node_id = bookmark_node_id.clone();
        let backfill_peer_label = bookmark_peer_label.clone();
        let (context, expected_attachment_id) = {
            let registry = self.registry.lock().await;
            (
                registry.remote_attachment_prepare_context(),
                registry.remote_attachment_id(&session_id),
            )
        };
        let backfill_sink = context.event_sink.clone();
        let prepared = crate::agent::session_registry::prepare_remote_attachment(
            context,
            session_id.clone(),
            remote_ref,
            peer_label,
            mesh,
            preferred_scope,
            remote_node_id,
        )
        .await?;
        let session_ref = prepared.session_ref().clone();
        let attachment_id = prepared.attachment_id();
        let health_timeout = Self::remote_connect_health_timeout();
        match tokio::time::timeout(health_timeout, session_ref.get_mode()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
                return Err(error);
            }
            Err(_) => {
                crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
                return Err(Self::remote_health_check_timeout_error(health_timeout));
            }
        }

        if let Some(node_id) = bookmark_node_id {
            use crate::session::store::{RemoteSessionBookmark, RemoteSessionBookmarkUpdate};
            let store = self.config.provider.history_store();
            let existing = match store.get_remote_session_bookmark(&session_id).await {
                Ok(existing) => existing,
                Err(error) => {
                    crate::agent::session_registry::abort_prepared_remote_attachment(prepared)
                        .await;
                    return Err(crate::error::AgentError::RemoteActor(error.to_string()));
                }
            };
            let bookmark = existing
                .map(|value| {
                    value.merge_confirmed(RemoteSessionBookmarkUpdate {
                        node_id: Some(node_id.clone()),
                        peer_label: Some(bookmark_peer_label.clone()),
                        cwd: None,
                        title: None,
                    })
                })
                .unwrap_or_else(|| RemoteSessionBookmark {
                    session_id: session_id.clone(),
                    node_id,
                    peer_label: bookmark_peer_label,
                    cwd: None,
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_secs() as i64)
                        .unwrap_or(0),
                    title: None,
                });
            if let Err(error) = store.save_remote_session_bookmark(&bookmark).await {
                crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
                return Err(crate::error::AgentError::RemoteActor(error.to_string()));
            }
        }

        let commit = {
            let mut registry = self.registry.lock().await;
            registry.install_remote_attachment(prepared, expected_attachment_id)
        };
        let old = match commit {
            Ok(old) => old,
            Err(boxed) => {
                let (prepared, conflict) = *boxed;
                crate::agent::session_registry::abort_prepared_remote_attachment(prepared).await;
                return Err(crate::error::AgentError::RemoteActor(format!(
                    "remote attachment changed while preparing session {} (expected={:?}, current={:?})",
                    session_id, conflict.expected_attachment_id, conflict.current_attachment_id
                )));
            }
        };
        if let Some(old) = old {
            crate::agent::session_registry::cleanup_installed_remote_attachment(old, true).await;
        }

        // Cursor-based backfill (plan §16): live subscription was established
        // during prepare; historical pages after the persisted source cursor
        // are recovered with overlap-safe deduplication. Only possible when a
        // stable node id is available (legacy attachments without one keep
        // the pre-Phase-10 behavior).
        if let Some(node_id) = backfill_node_id {
            tokio::spawn(
                crate::agent::remote::event_backfill::backfill_remote_events(
                    backfill_sink,
                    session_ref.clone(),
                    session_id.clone(),
                    node_id,
                    backfill_peer_label,
                    attachment_id,
                ),
            );
        }

        Ok(session_ref)
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn detach_remote_session_attachment(
        &self,
        session_id: &str,
        notify_remote: bool,
    ) -> Option<crate::agent::remote::SessionActorRef> {
        let attachment = {
            let mut registry = self.registry.lock().await;
            registry.take_remote_attachment(session_id)
        }?;
        let session_ref = attachment.session_ref.clone();
        crate::agent::session_registry::cleanup_installed_remote_attachment(
            attachment,
            notify_remote,
        )
        .await;
        Some(session_ref)
    }

    #[cfg(feature = "remote")]
    pub async fn resume_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::ResumeRemoteSession;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &ResumeRemoteSession { session_id },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    #[cfg(feature = "remote")]
    pub async fn create_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        request: crate::agent::remote::CreateRemoteSchedule,
    ) -> Result<crate::agent::remote::CreateRemoteScheduleResponse, agent_client_protocol::Error>
    {
        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(node_manager_ref, &request, timeout)
            .await
            .map_err(Self::map_remote_node_manager_error)
    }

    #[cfg(feature = "remote")]
    pub async fn list_remote_schedules(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: Option<String>,
    ) -> Result<crate::agent::remote::ListRemoteSchedulesResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::ListRemoteSchedules;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &ListRemoteSchedules { session_id },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    #[cfg(feature = "remote")]
    pub async fn pause_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::PauseRemoteSchedule;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &PauseRemoteSchedule { schedule_public_id },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    #[cfg(feature = "remote")]
    pub async fn resume_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::ResumeRemoteSchedule;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &ResumeRemoteSchedule { schedule_public_id },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    #[cfg(feature = "remote")]
    pub async fn trigger_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::TriggerRemoteSchedule;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &TriggerRemoteSchedule { schedule_public_id },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    #[cfg(feature = "remote")]
    pub async fn delete_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::DeleteRemoteSchedule;

        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &DeleteRemoteSchedule { schedule_public_id },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }
}

#[cfg(all(test, feature = "remote"))]
mod tests {
    use super::*;

    /// Regression test: the remote attachment health-check timeout must
    /// surface as a typed `RemoteTransport` failure (ReplyTimeout kind, Unknown
    /// delivery) so `transport_failure()` stays usable for recovery decisions,
    /// instead of the untyped `AgentError::RemoteActor` string previously
    /// returned by `attach_remote_session`.
    ///
    /// `attach_remote_session` applies this mapping when its
    /// `tokio::time::timeout(remote_connect_health_timeout(), get_mode())`
    /// guard expires. Driving a real get_mode() past the deadline end-to-end
    /// requires a live mesh peer (this crate has no offline remote-transport
    /// test harness), so the typed construction that timeout path produces is
    /// asserted directly here.
    #[test]
    fn health_check_timeout_maps_to_typed_reply_timeout_failure() {
        let error = LocalAgentHandle::remote_health_check_timeout_error(
            std::time::Duration::from_millis(50),
        );
        let failure = error
            .transport_failure()
            .expect("health-check timeout must carry a typed transport failure");
        assert!(matches!(
            failure.kind,
            querymt_remote::RemoteTransportFailureKind::ReplyTimeout
        ));
        assert_eq!(failure.delivery, querymt_remote::DeliveryCertainty::Unknown);
        assert!(
            failure.is_retryable_kind(),
            "reply timeouts must stay retryable for recovery decisions"
        );
        assert!(
            error.to_string().contains("health check timed out"),
            "message should name the timeout, got: {}",
            error
        );
    }
}
