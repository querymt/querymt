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

    #[cfg(feature = "remote")]
    fn map_remote_control_error(
        error: kameo::error::RemoteSendError<agent_client_protocol::Error>,
    ) -> agent_client_protocol::Error {
        match querymt_remote::classify_remote_send_error(error) {
            Ok(failure) => crate::error::AgentError::from_transport_failure(failure).into(),
            Err(error) => error,
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn delegate_session_owner(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, Error> {
        let store = self.config.provider.history_store();
        match crate::session::location::resolve_session_location(store.as_ref(), session_id.trim())
            .await
            .map_err(Error::into_internal_error)?
        {
            crate::session::location::SessionLocation::Remote { bookmark } => {
                Ok(Some(bookmark.node_id))
            }
            crate::session::location::SessionLocation::Conflict { .. } => {
                Err(Error::invalid_params().data("session location conflict"))
            }
            _ => Ok(None),
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn remote_profiles(&self, node_id: &str) -> Result<serde_json::Value, Error> {
        let manager = self.find_node_manager(node_id).await?;
        let mut response = querymt_remote::ask_remote_with_timeout(
            &manager,
            &crate::agent::remote::node_manager::ListRemoteProfiles,
            Self::remote_request_timeout(),
        )
        .await
        .map_err(Self::map_remote_control_error)?;
        response["node_id"] = node_id.into();
        Ok(response)
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn remote_delegate_models(
        &self,
        node_id: &str,
        request: crate::control::delegate_models::DelegateModelsRequest,
    ) -> Result<crate::control::delegate_models::DelegateAssignmentsInfo, Error> {
        let manager = self.find_node_manager(node_id).await?;
        let mut result = querymt_remote::ask_remote_with_timeout(
            &manager,
            &crate::agent::remote::node_manager::GetRemoteDelegateModels(request),
            Self::remote_request_timeout(),
        )
        .await
        .map_err(Self::map_remote_control_error)?;
        let viewer = self.mesh().map(|mesh| mesh.peer_id().to_string());
        for assignment in &mut result.assignments {
            Self::qualify_owner_model(&mut assignment.model.0, node_id, viewer.as_deref());
        }
        for assignment in &mut result.orphaned_overrides {
            Self::qualify_owner_model(&mut assignment.model.0, node_id, viewer.as_deref());
        }
        Ok(result)
    }

    #[cfg(feature = "remote")]
    fn qualify_owner_model(
        model: &mut Option<crate::delegation::DelegateModelOverride>,
        node_id: &str,
        viewer: Option<&str>,
    ) {
        if let Some(model) = model {
            model.node_id = match model.node_id.as_deref() {
                None => Some(node_id.to_owned()),
                Some(node) if Some(node) == viewer => None,
                _ => model.node_id.clone(),
            };
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) async fn remote_set_delegate_model(
        &self,
        node_id: &str,
        mut request: crate::control::delegate_models::SetDelegateModelRequest,
    ) -> Result<crate::control::delegate_models::SetDelegateModelResponse, Error> {
        // In the client catalog an unqualified model is local to this viewer,
        // not to the owner receiving the forwarded request.
        if request.model_id.0.is_some() && request.node_id.is_none() {
            request.node_id = Some(
                self.mesh()
                    .ok_or_else(Error::method_not_found)?
                    .peer_id()
                    .to_string(),
            );
        }
        let manager = self.find_node_manager(node_id).await?;
        let mut result = querymt_remote::ask_remote_with_timeout(
            &manager,
            &crate::agent::remote::node_manager::SetRemoteDelegateModel(request),
            Self::remote_request_timeout(),
        )
        .await
        .map_err(Self::map_remote_control_error)?;
        Self::qualify_owner_model(
            &mut result.model.0,
            node_id,
            self.mesh()
                .map(|mesh| mesh.peer_id().to_string())
                .as_deref(),
        );
        Ok(result)
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

    /// List a filtered owner page and its authoritative lifecycle metadata.
    #[cfg(feature = "remote")]
    pub(crate) async fn list_remote_sessions_with_lifecycle(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        offset: Option<u32>,
        limit: Option<u32>,
        cwd: Option<String>,
        scope: crate::session::projection::SessionScope,
        lifecycle: crate::session::lifecycle::SessionLifecycleFilter,
    ) -> Result<
        crate::agent::remote::node_manager::ListRemoteSessionsWithLifecycleResponse,
        agent_client_protocol::Error,
    > {
        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &crate::agent::remote::ListRemoteSessionsWithLifecycle {
                offset,
                limit,
                cwd,
                scope,
                lifecycle,
            },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    /// Read lifecycle state from the authoritative owner without attaching an actor.
    #[cfg(feature = "remote")]
    pub(crate) async fn get_remote_session_lifecycle(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
    ) -> Result<crate::session::lifecycle::SessionLifecycle, agent_client_protocol::Error> {
        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &crate::agent::remote::GetRemoteSessionLifecycle { session_id },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    /// Patch lifecycle state on the authoritative owner without attaching an actor.
    #[cfg(feature = "remote")]
    pub(crate) async fn patch_remote_session_lifecycle(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
        patch: crate::session::lifecycle::SessionLifecyclePatch,
    ) -> Result<crate::session::lifecycle::SessionLifecycle, agent_client_protocol::Error> {
        let timeout = Self::remote_request_timeout();
        querymt_remote::ask_remote_with_timeout(
            node_manager_ref,
            &crate::agent::remote::SetRemoteSessionLifecycle { session_id, patch },
            timeout,
        )
        .await
        .map_err(Self::map_remote_node_manager_error)
    }

    /// Read display-only profile identity from the session's owning node.
    #[cfg(feature = "remote")]
    pub(crate) async fn remote_session_profile(
        &self,
        node_id: &str,
        session_id: &str,
    ) -> Option<Option<crate::agent::remote::RemoteSessionProfile>> {
        use crate::agent::remote::GetRemoteSessionProfile;
        // Missing metadata must not block loading a healthy remote session.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let manager = self.find_node_manager(node_id).await.ok()?;
            querymt_remote::ask_remote_with_timeout(
                &manager,
                &GetRemoteSessionProfile {
                    session_id: session_id.to_string(),
                },
                Self::remote_request_timeout(),
            )
            .await
            .ok()
        })
        .await
        .ok()
        .flatten()
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
        self.create_remote_session_with_profile(node_manager_ref, cwd, None)
            .await
    }

    #[cfg(feature = "remote")]
    pub async fn create_remote_session_with_profile(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        cwd: Option<String>,
        profile_id: Option<String>,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::CreateRemoteSession;
        if let Some(profile_id) = profile_id {
            return querymt_remote::ask_remote_with_timeout(
                node_manager_ref,
                &crate::agent::remote::node_manager::CreateRemoteSessionWithProfile {
                    cwd,
                    profile_id,
                },
                Self::remote_request_timeout(),
            )
            .await
            .map_err(Self::map_remote_node_manager_error);
        }
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
        let node_id = remote_node_id
            .or_else(|| remote_ref.id().peer_id().map(|peer| peer.to_string()))
            .ok_or_else(|| {
                crate::error::AgentError::Internal("remote actor has no peer identity".into())
            })?;
        self.connect_remote_session(
            &session_id,
            super::remote_connect::RemoteConnectOptions {
                node_hint: Some(&node_id),
                peer_label: Some(&peer_label),
                preferred_scope,
                reason: super::remote_connect::RemoteConnectReason::ExtensionAttach,
                replace: super::remote_connect::RemoteReplacePolicy::ReplaceCurrent,
                handoff: Some(
                    crate::agent::remote::node_manager::SessionHandoff::DirectRemote {
                        session_ref: remote_ref,
                    },
                ),
            },
        )
        .await
        .map(|connected| connected.session_ref)
        .map_err(|error| crate::error::AgentError::RemoteActor(error.to_string()))
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

    #[test]
    fn delegate_models_preserve_physical_provider_nodes_across_owner_and_viewer() {
        for (reported, expected) in [
            (None, Some("B")),
            (Some("A"), None),
            (Some("B"), Some("B")),
            (Some("C"), Some("C")),
        ] {
            let mut model = Some(crate::delegation::DelegateModelOverride {
                model_id: "provider/model".into(),
                node_id: reported.map(str::to_owned),
            });
            LocalAgentHandle::qualify_owner_model(&mut model, "B", Some("A"));
            assert_eq!(model.unwrap().node_id.as_deref(), expected);
        }
        let mut inherited = None;
        LocalAgentHandle::qualify_owner_model(&mut inherited, "B", Some("A"));
        assert!(inherited.is_none());
    }

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
