use super::*;

impl LocalAgentHandle {
    #[cfg(feature = "remote")]
    fn map_remote_node_manager_error(
        error: kameo::error::RemoteSendError<crate::error::AgentError>,
    ) -> agent_client_protocol::Error {
        use crate::error::AgentError;

        match error {
            kameo::error::RemoteSendError::HandlerError(err) => {
                agent_client_protocol::Error::from(err)
            }
            other => agent_client_protocol::Error::from(AgentError::RemoteActor(other.to_string())),
        }
    }

    /// Find a `RemoteNodeManager` by its stable node id (PeerId string).
    ///
    /// ## Fast path
    ///
    /// Tries a direct DHT lookup under the scoped per-peer node-manager name first.
    /// This succeeds whenever the remote node registered under the same scope
    /// (see [`crate::agent::remote::scope::scoped_node_manager_for_peer`]) and is **not** gated on
    /// `is_peer_alive`, so it works even when mDNS has transiently expired the
    /// peer (TTL = 30 s) while the TCP connection is still alive.
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
        for scope in runtime.active_scopes() {
            let direct_dht_name =
                crate::agent::remote::scope::scoped_node_manager_for_peer(&scope, &node_id);
            match runtime
                .lookup_actor::<RemoteNodeManager>(direct_dht_name.clone())
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
                            let res = tokio::time::timeout(
                                timeout,
                                node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                            )
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
                Ok(Ok(info)) => {
                    self.insert_cached_remote_node(cache_key, info.clone());
                    if info.node_id.to_string() == node_id {
                        return Ok(node_manager_ref);
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("find_node_manager: GetNodeInfo failed: {}", e);
                }
                Err(_) => {
                    log::warn!(
                        "find_node_manager: GetNodeInfo timed out for peer {:?}",
                        peer_id
                    );
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
        node_manager_ref
            .ask(&ListRemoteSessions { offset, limit })
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

        node_manager_ref
            .ask(&CreateRemoteSession { cwd })
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

        node_manager_ref
            .ask(&ForkRemoteSession {
                source_session_id,
                message_id,
            })
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
    ) -> crate::agent::remote::SessionActorRef {
        let mesh = self.mesh();
        let mut registry = self.registry.lock().await;
        registry
            .attach_remote_session(
                session_id,
                remote_ref,
                peer_label,
                mesh,
                preferred_scope,
                remote_node_id,
            )
            .await
    }

    #[cfg(feature = "remote")]
    pub async fn resume_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::ResumeRemoteSession;

        node_manager_ref
            .ask(&ResumeRemoteSession { session_id })
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
        node_manager_ref
            .ask(&request)
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

        node_manager_ref
            .ask(&ListRemoteSchedules { session_id })
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

        node_manager_ref
            .ask(&PauseRemoteSchedule { schedule_public_id })
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

        node_manager_ref
            .ask(&ResumeRemoteSchedule { schedule_public_id })
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

        node_manager_ref
            .ask(&TriggerRemoteSchedule { schedule_public_id })
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

        node_manager_ref
            .ask(&DeleteRemoteSchedule { schedule_public_id })
            .await
            .map_err(Self::map_remote_node_manager_error)
    }
}
