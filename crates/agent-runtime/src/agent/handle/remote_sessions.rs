use super::*;

impl LocalAgentHandle {
    /// Ask a remote node manager over the protocol negotiated with its node.
    #[cfg(feature = "remote")]
    pub(super) async fn ask_node_manager<M, T>(
        &self,
        node: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        request: &M,
    ) -> Result<
        <<crate::agent::remote::RemoteNodeManager as kameo::message::Message<M>>::Reply as kameo::reply::Reply>::Ok,
        T,
    >
    where
        crate::agent::remote::RemoteNodeManager:
            kameo::message::Message<M> + kameo::remote::RemoteMessage<M>,
        M: serde::Serialize + Send + Sync + 'static,
        <<crate::agent::remote::RemoteNodeManager as kameo::message::Message<M>>::Reply as kameo::reply::Reply>::Ok:
            serde::de::DeserializeOwned,
        <<crate::agent::remote::RemoteNodeManager as kameo::message::Message<M>>::Reply as kameo::reply::Reply>::Error:
            serde::de::DeserializeOwned + std::fmt::Debug + std::fmt::Display + Into<T>,
        T: crate::agent::remote::remote_call::RemoteCallError + From<crate::error::AgentError>,
{
        use crate::agent::remote::remote_call::{CallTimeouts, ask_remote};
        let mesh = self
            .mesh()
            .ok_or(crate::error::AgentError::MeshNotBootstrapped)?;
        ask_remote(
            Some(&mesh),
            node,
            request,
            CallTimeouts::uniform(Self::remote_request_timeout()),
        )
        .await
    }

    /// Create a session for a delegation. The owner records it with fork
    /// origin `delegation`.
    #[cfg(feature = "remote")]
    pub(crate) async fn create_remote_delegation_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        cwd: Option<String>,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, agent_client_protocol::Error>
    {
        self.ask_node_manager(
            node_manager_ref,
            &crate::agent::remote::CreateRemoteDelegationSession { cwd },
        )
        .await
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
        let mut response = self
            .ask_node_manager::<_, Error>(
                &manager,
                &crate::agent::remote::node_manager::ListRemoteProfiles,
            )
            .await?;
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
        let mut result = self
            .ask_node_manager::<_, Error>(
                &manager,
                &crate::agent::remote::node_manager::GetRemoteDelegateModels(request),
            )
            .await?;
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
        let mut result = self
            .ask_node_manager::<_, Error>(
                &manager,
                &crate::agent::remote::node_manager::SetRemoteDelegateModel(request),
            )
            .await?;
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
        use crate::agent::remote::RemoteNodeManager;
        use futures_util::StreamExt;

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
                Ok(Some(node_manager_ref))
                    if node_manager_ref.id().peer_id() == parsed_peer_id.as_ref() =>
                {
                    log::debug!(
                        "find_node_manager: fast-path DHT hit for '{}'",
                        direct_dht_name
                    );
                    return Ok(node_manager_ref);
                }
                Ok(_) => {
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
        let mut probes = super::remote_nodes::NodeInfoProbes::new(mesh.clone());

        for scope in runtime.active_scopes() {
            let mut stream = runtime.lookup_all_actors::<RemoteNodeManager>(
                crate::agent::remote::scope::scoped_node_manager(&scope),
            );
            while let Some(result) = stream.next().await {
                let node_manager_ref = match result {
                    Ok(node_manager_ref) => node_manager_ref,
                    Err(e) => {
                        log::warn!("find_node_manager: lookup error: {}", e);
                        continue;
                    }
                };
                let peer_id = node_manager_ref.id().peer_id().copied();
                if peer_id == Some(local_peer_id)
                    || parsed_peer_id.is_some_and(|expected| peer_id != Some(expected))
                {
                    continue;
                }
                // No is_peer_alive check here — we contact the peer
                // directly and let the GetNodeInfo timeout decide.
                let cache_key = Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                if let Some(info) = self.get_cached_remote_node(&cache_key) {
                    if info.node_id.to_string() == node_id {
                        return Ok(node_manager_ref);
                    }
                    continue;
                }
                probes.push(node_manager_ref, cache_key);
            }
        }

        while let Some(probe) = probes.next().await {
            if let Ok(Some(info)) = probe.result {
                let matches = info.node_id.to_string() == node_id;
                self.insert_cached_remote_node(probe.cache_key, info);
                if matches {
                    return Ok(probe.node_manager);
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
        self.ask_node_manager(node_manager_ref, &ListRemoteSessions { offset, limit })
            .await
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
        self.ask_node_manager(
            node_manager_ref,
            &crate::agent::remote::ListRemoteSessionsWithLifecycle {
                offset,
                limit,
                cwd,
                scope,
                lifecycle,
            },
        )
        .await
    }

    /// Read lifecycle state from the authoritative owner without attaching an actor.
    #[cfg(feature = "remote")]
    pub(crate) async fn get_remote_session_lifecycle(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
    ) -> Result<crate::session::lifecycle::SessionLifecycle, agent_client_protocol::Error> {
        self.ask_node_manager(
            node_manager_ref,
            &crate::agent::remote::GetRemoteSessionLifecycle { session_id },
        )
        .await
    }

    /// Patch lifecycle state on the authoritative owner without attaching an actor.
    #[cfg(feature = "remote")]
    pub(crate) async fn patch_remote_session_lifecycle(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
        patch: crate::session::lifecycle::SessionLifecyclePatch,
    ) -> Result<crate::session::lifecycle::SessionLifecycle, agent_client_protocol::Error> {
        self.ask_node_manager(
            node_manager_ref,
            &crate::agent::remote::SetRemoteSessionLifecycle { session_id, patch },
        )
        .await
    }

    /// Read display-only profile identity from the session's owning node,
    /// through `node_manager` when the caller already found it.
    #[cfg(feature = "remote")]
    pub(crate) async fn remote_session_profile(
        &self,
        node_id: &str,
        session_id: &str,
        node_manager: Option<super::model_registry::NodeManagerRef>,
    ) -> Option<Option<crate::agent::remote::RemoteSessionProfile>> {
        use crate::agent::remote::GetRemoteSessionProfile;
        // Missing metadata must not block loading a healthy remote session.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let manager = match node_manager {
                Some(manager) => manager,
                None => self.find_node_manager(node_id).await.ok()?,
            };
            self.ask_node_manager::<_, crate::error::AgentError>(
                &manager,
                &GetRemoteSessionProfile {
                    session_id: session_id.to_string(),
                },
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
            return self
                .ask_node_manager(
                    node_manager_ref,
                    &crate::agent::remote::node_manager::CreateRemoteSessionWithProfile {
                        cwd,
                        profile_id,
                    },
                )
                .await;
        }
        self.ask_node_manager(node_manager_ref, &CreateRemoteSession { cwd })
            .await
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

        self.fork_remote_session_request(
            node_manager_ref,
            ForkRemoteSession {
                source_session_id,
                message_id: Some(message_id),
                cursor_id: None,
            },
        )
        .await
    }

    /// Send a fork request, with its target, to the node that owns the source.
    ///
    /// Goes through the sharing protocol so the owner sees this node's peer
    /// authority and applies its grants.
    #[cfg(feature = "remote")]
    pub(crate) async fn fork_remote_session_request(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        request: crate::agent::remote::ForkRemoteSession,
    ) -> Result<crate::agent::remote::ForkRemoteSessionResponse, agent_client_protocol::Error> {
        self.ask_node_manager(node_manager_ref, &request).await
    }

    /// `session/fork` of a session another node owns: the owner forks (so the
    /// fork is remote, on the owner) and this node attaches it, which gives it
    /// a bookmark so `session/load` finds it.
    ///
    /// The target follows the local rules; the owner resolves it against its
    /// own history. An undecodable or non-forkable cursor is rejected here,
    /// before any mesh call.
    ///
    /// Workspace options in the request's `_meta` are rejected with `-32602`
    /// rather than dropped: they do not travel over the mesh, and a dropped
    /// isolation request would silently yield a shared checkout, which a local
    /// fork refuses to do.
    ///
    /// Once the owner has created the fork it is a durable session there, as a
    /// local fork is, so the viewer always records its bookmark. A failed
    /// attach is then only logged: `session/load` repairs the connection from
    /// the bookmark.
    #[cfg(feature = "remote")]
    pub(crate) async fn fork_remote_hosted_session(
        &self,
        node_id: &str,
        req: crate::acp::protocol::ForkSessionRequest,
    ) -> Result<crate::acp::protocol::ForkSessionResponse, agent_client_protocol::Error> {
        use super::remote_connect::{
            RemoteConnectOptions, RemoteConnectReason, RemoteReplacePolicy,
        };
        use crate::agent::session_materializer::{
            fork_message_id_from_meta, forkable_cursor_message_id,
        };
        use crate::session::domain::ForkOrigin;
        use crate::session::store::RemoteSessionBookmark;

        let cursor_id = req.cursor_id.as_ref().map(|cursor| cursor.0.to_string());
        if let Some(cursor_id) = &cursor_id {
            forkable_cursor_message_id(cursor_id)?;
        }
        if crate::worktree::fork_creation_options_from_meta(req.meta.as_ref())
            .map_err(|message| {
                agent_client_protocol::Error::invalid_params().data(serde_json::json!({
                    "code": "invalid_options",
                    "message": message,
                }))
            })?
            .is_some()
        {
            return Err(
                agent_client_protocol::Error::invalid_params().data(serde_json::json!({
                    "code": "remote_fork_workspace_options",
                    "message": "workspace options are not supported when forking a session \
                                hosted on another node; the owner applies its own workspace \
                                policy",
                })),
            );
        }
        // MCP servers do not travel over the mesh either; dropping them would
        // fork without the tools the client asked for.
        if !req.mcp_servers.is_empty() {
            return Err(
                agent_client_protocol::Error::invalid_params().data(serde_json::json!({
                    "code": "remote_fork_mcp_servers",
                    "message": "MCP servers are not supported when forking a session hosted \
                                on another node",
                })),
            );
        }
        // A cursor wins on the owner; the hint is sent only without one, and
        // the cursor's message id is not sent as a hint, so a node that does
        // not know cursors fails the request rather than forking elsewhere.
        let message_id = match cursor_id {
            Some(_) => None,
            None => fork_message_id_from_meta(req.meta.as_ref()).map(ToOwned::to_owned),
        };
        let source_session_id = req.session_id.to_string();
        let store = self.config.provider.history_store();
        // The fork lives on the source's node, so the source's label is the
        // fork's label.
        let peer_label = store
            .get_remote_session_bookmark(&source_session_id)
            .await
            .ok()
            .flatten()
            .map(|bookmark| bookmark.peer_label);
        let node_manager = self.find_node_manager(node_id).await?;
        let response = self
            .fork_remote_session_request(
                &node_manager,
                crate::agent::remote::ForkRemoteSession {
                    source_session_id,
                    message_id,
                    cursor_id,
                },
            )
            .await?;
        let crate::agent::remote::ForkRemoteSessionResponse {
            session_id: forked_session_id,
            handoff,
            cwd,
            title,
            created_at,
        } = response;

        let attached = self
            .connect_remote_session(
                &forked_session_id,
                RemoteConnectOptions {
                    node_hint: Some(node_id),
                    peer_label: peer_label.as_deref(),
                    preferred_scope: None,
                    reason: RemoteConnectReason::ExtensionAttach,
                    replace: RemoteReplacePolicy::ReuseIfPresent,
                    handoff: Some(handoff),
                    windowed: false,
                    fork_origin: Some(ForkOrigin::User),
                },
            )
            .await;
        if let Err(error) = attached {
            log::warn!(
                "remote fork {forked_session_id} on node {node_id} was created but not attached: \
                 {error}; recording its bookmark so session/load can connect it"
            );
        }

        // Record the fork with the owner's metadata: the attach above writes
        // a bookmark without cwd or title, and none at all when it failed.
        let unrecorded = |error: String| {
            agent_client_protocol::Error::internal_error().data(serde_json::json!({
                "message": "the fork was created on the owning node but could not be \
                            recorded on this node",
                "forkedSessionId": forked_session_id,
                "nodeId": node_id,
                "error": error,
            }))
        };
        let mut bookmark = match store.get_remote_session_bookmark(&forked_session_id).await {
            Ok(Some(bookmark)) => bookmark,
            Ok(None) => RemoteSessionBookmark {
                session_id: forked_session_id.clone(),
                node_id: node_id.to_owned(),
                peer_label: peer_label.unwrap_or_else(|| node_id.to_owned()),
                cwd: None,
                created_at,
                title: None,
                fork_origin: None,
            },
            Err(error) => return Err(unrecorded(error.to_string())),
        };
        bookmark.created_at = created_at;
        bookmark.cwd = cwd.or(bookmark.cwd);
        bookmark.title = title.or(bookmark.title);
        bookmark.fork_origin = Some(ForkOrigin::User);
        store
            .save_remote_session_bookmark(&bookmark)
            .await
            .map_err(|error| unrecorded(error.to_string()))?;
        Ok(crate::acp::protocol::ForkSessionResponse::new(
            forked_session_id,
        ))
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
                windowed: false,
                fork_origin: None,
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

    /// Re-materialize a persisted session on its owner. Failures stay typed so
    /// recovery can decide from the transport failure.
    #[cfg(feature = "remote")]
    pub async fn resume_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: String,
    ) -> Result<crate::agent::remote::CreateRemoteSessionResponse, crate::error::AgentError> {
        use crate::agent::remote::ResumeRemoteSession;

        self.ask_node_manager(node_manager_ref, &ResumeRemoteSession { session_id })
            .await
    }

    #[cfg(feature = "remote")]
    pub async fn create_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        request: crate::agent::remote::CreateRemoteSchedule,
    ) -> Result<crate::agent::remote::CreateRemoteScheduleResponse, agent_client_protocol::Error>
    {
        self.ask_node_manager(node_manager_ref, &request).await
    }

    #[cfg(feature = "remote")]
    pub async fn list_remote_schedules(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        session_id: Option<String>,
    ) -> Result<crate::agent::remote::ListRemoteSchedulesResponse, agent_client_protocol::Error>
    {
        use crate::agent::remote::ListRemoteSchedules;

        self.ask_node_manager(node_manager_ref, &ListRemoteSchedules { session_id })
            .await
    }

    #[cfg(feature = "remote")]
    pub async fn pause_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::PauseRemoteSchedule;

        self.ask_node_manager(
            node_manager_ref,
            &PauseRemoteSchedule { schedule_public_id },
        )
        .await
    }

    #[cfg(feature = "remote")]
    pub async fn resume_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::ResumeRemoteSchedule;

        self.ask_node_manager(
            node_manager_ref,
            &ResumeRemoteSchedule { schedule_public_id },
        )
        .await
    }

    #[cfg(feature = "remote")]
    pub async fn trigger_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::TriggerRemoteSchedule;

        self.ask_node_manager(
            node_manager_ref,
            &TriggerRemoteSchedule { schedule_public_id },
        )
        .await
    }

    #[cfg(feature = "remote")]
    pub async fn delete_remote_schedule(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        schedule_public_id: String,
    ) -> Result<(), agent_client_protocol::Error> {
        use crate::agent::remote::DeleteRemoteSchedule;

        self.ask_node_manager(
            node_manager_ref,
            &DeleteRemoteSchedule { schedule_public_id },
        )
        .await
    }

    /// Finds the node manager of a remote member's peer. The node is resolved
    /// now, not when the member was listed; a failure names the peer.
    #[cfg(feature = "remote")]
    async fn remote_member_node(
        &self,
        member: &crate::delegation::roster::RemoteMember,
    ) -> Result<
        (
            String,
            kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        ),
        agent_client_protocol::Error,
    > {
        let peer = member.peer();
        let mesh = self.mesh().ok_or_else(|| {
            agent_client_protocol::Error::new(
                -32001,
                format!("Remote peer '{peer}' is unreachable: the mesh is not running"),
            )
        })?;
        // The mesh's own resolution wins over the owner id taken from the
        // configured address.
        let node_id = match mesh.resolve_peer_node_id(peer).await {
            Some(node_id) => node_id.to_string(),
            None => member.node_id().map(str::to_owned).ok_or_else(|| {
                agent_client_protocol::Error::new(
                    -32001,
                    format!("Remote peer '{peer}' could not be resolved to a live node id"),
                )
            })?,
        };
        let node_manager = self.find_node_manager(&node_id).await.map_err(|error| {
            agent_client_protocol::Error::new(
                -32001,
                format!("Remote peer '{peer}' is unreachable: {}", error.message),
            )
        })?;
        Ok((node_id, node_manager))
    }

    /// Opens a delegated session on a remote member and attaches it to this
    /// runtime: the calls of `control::remote::create_remote_session` with
    /// `attach = true`, under `RemoteConnectReason::Delegation`. The session's
    /// events are then relayed into this runtime's event stream.
    #[cfg(feature = "remote")]
    pub(crate) async fn open_remote_delegation_session(
        &self,
        member: &crate::delegation::roster::RemoteMember,
        cwd: Option<String>,
    ) -> Result<(String, crate::agent::remote::SessionActorRef), agent_client_protocol::Error> {
        use super::remote_connect::{RemoteConnectOptions, RemoteConnectReason};

        let (node_id, node_manager) = self.remote_member_node(member).await?;
        // The owner records the session with fork origin `delegation`; the
        // bookmark records the same, so neither side lists it as a user session.
        let response = self
            .create_remote_delegation_session(&node_manager, cwd)
            .await?;
        let reason = RemoteConnectReason::Delegation;
        let connected = self
            .connect_remote_session(
                &response.session_id,
                RemoteConnectOptions {
                    node_hint: Some(&node_id),
                    peer_label: Some(member.peer()),
                    preferred_scope: None,
                    reason,
                    replace: reason.default_replace_policy(),
                    handoff: Some(response.handoff),
                    windowed: false,
                    fork_origin: Some(crate::session::domain::ForkOrigin::Delegation),
                },
            )
            .await
            .map_err(|error| error.to_acp_error())?;
        Ok((response.session_id, connected.session_ref))
    }

    /// Sets lifecycle state of a session owned by a remote member's node.
    #[cfg(feature = "remote")]
    pub(crate) async fn set_remote_member_session_lifecycle(
        &self,
        member: &crate::delegation::roster::RemoteMember,
        session_id: &str,
        patch: crate::session::lifecycle::SessionLifecyclePatch,
    ) -> crate::session::error::SessionResult<crate::session::lifecycle::SessionLifecycle> {
        use crate::session::error::SessionError;

        patch.validate()?;
        if session_id.is_empty() {
            return Err(SessionError::InvalidOperation(
                "session id must not be empty".into(),
            ));
        }
        let (_, node_manager) = self
            .remote_member_node(member)
            .await
            .map_err(|error| SessionError::InvalidOperation(error.message))?;
        self.patch_remote_session_lifecycle(&node_manager, session_id.to_owned(), patch)
            .await
            .map_err(|error| SessionError::InvalidOperation(error.message))
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
