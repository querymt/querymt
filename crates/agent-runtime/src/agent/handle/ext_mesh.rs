use super::utils::ext_json_response;
use super::*;

pub(super) const ACCESS_METHODS: [&str; 4] = [
    "querymt/mesh/getPeerAccess",
    "querymt/mesh/setPeerAccess",
    "querymt/mesh/setPeerQuota",
    "querymt/mesh/resetPeerQuota",
];

impl LocalAgentHandle {
    pub(super) async fn handle_ext_mesh_status(&self) -> Result<ExtResponse, Error> {
        ext_json_response(&crate::control::mesh::status(self).await)
    }

    pub(super) async fn handle_ext_mesh_join(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        let parsed: crate::control::mesh::MeshJoinRequest = serde_json::from_str(req.params.get())
            .map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::mesh::join(self, parsed).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_mesh_nodes(&self, owner: bool) -> Result<ExtResponse, Error> {
        let nodes = crate::control::mesh::list_nodes(self).await;
        #[cfg(feature = "remote")]
        if owner
            && let Some(mesh) = self.mesh()
            && let Some(store) = mesh.quota_store()
        {
            let host = querymt_remote::NodeId::from_peer_id(*mesh.peer_id());
            let peers = store
                .managed_peers(&host)
                .await
                .map_err(access::grant_error)?;
            let ids: Vec<_> = peers.iter().map(ToString::to_string).collect();
            return ext_json_response(
                &serde_json::json!({"nodes": nodes.nodes, "managed_peer_ids": ids}),
            );
        }
        let _ = owner;
        ext_json_response(&nodes)
    }

    pub(super) fn has_mesh_access_store(&self) -> bool {
        #[cfg(feature = "remote")]
        {
            self.mesh().is_some_and(|mesh| mesh.quota_store().is_some())
        }
        #[cfg(not(feature = "remote"))]
        {
            false
        }
    }

    pub(super) async fn handle_ext_mesh_access(
        &self,
        req: ExtRequest,
        owner: bool,
    ) -> Result<ExtResponse, Error> {
        if !owner {
            return Err(Error::new(-32000, "local owner authority required")
                .data(serde_json::json!({"code": "mesh_access_owner_required"})));
        }
        #[cfg(feature = "remote")]
        {
            access::dispatch(self, req).await
        }
        #[cfg(not(feature = "remote"))]
        {
            let _ = req;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_mesh_create_invite(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::mesh::CreateMeshInviteRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::mesh::create_invite(self, parsed).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_mesh_list_invites(&self) -> Result<ExtResponse, Error> {
        let response = crate::control::mesh::list_invites(self).await?;
        ext_json_response(&response)
    }

    pub(super) async fn handle_ext_mesh_revoke_invite(
        &self,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let parsed: crate::control::mesh::RevokeMeshInviteRequest =
            serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;
        let response = crate::control::mesh::revoke_invite(self, parsed).await?;
        ext_json_response(&response)
    }
}

#[cfg(feature = "remote")]
pub(crate) use access::register_peer_access_read;

#[cfg(feature = "remote")]
mod access {
    use super::*;
    use querymt_remote::{
        NodeId,
        peer_access::{
            GrantStoreError, ModelSelection, PeerGrant, PeerGrantKey, ProviderGrant,
            SharingProtocol,
            quota::{QuotaEdit, QuotaError, QuotaLimit, QuotaScope, QuotaStatus, QuotaWindow},
        },
    };
    use serde::{Deserialize, Serialize, de::DeserializeOwned};
    use serde_json::json;
    use typeshare::typeshare;
    use uuid::Uuid;

    #[typeshare]
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GetMeshPeerAccessRequest {
        version: u32,
        peer_id: String,
    }

    #[typeshare]
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SetMeshPeerAccessRequest {
        version: u32,
        peer_id: String,
        #[typeshare(serialized_as = "String")]
        expected_revision: Uuid,
        grant: PeerGrant,
    }

    #[typeshare]
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SetMeshPeerQuotaRequest {
        version: u32,
        peer_id: String,
        #[typeshare(serialized_as = "String")]
        expected_revision: Uuid,
        scope: QuotaScope,
        operation: MeshQuotaOperation,
        limit: MeshQuotaLimit,
    }

    #[typeshare]
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ResetMeshPeerQuotaRequest {
        version: u32,
        peer_id: String,
        #[typeshare(serialized_as = "String")]
        expected_revision: Uuid,
        scope: QuotaScope,
    }

    #[typeshare]
    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum MeshQuotaOperation {
        Update,
        Reconfigure,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum MeshQuotaLimit {
        Unlimited,
        Finite {
            limit_tokens: String,
            #[serde(default)]
            window: QuotaWindow,
        },
    }

    #[typeshare]
    #[derive(Serialize, Deserialize)]
    struct MeshQuotaUsage {
        known_tokens: String,
        estimated_tokens: String,
        has_unknown_usage: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reserved_tokens: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        available_tokens: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        window_started_at: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_reset_at: Option<String>,
    }

    #[typeshare]
    #[derive(Serialize, Deserialize)]
    struct MeshPeerQuota {
        scope: QuotaScope,
        limit: MeshQuotaLimit,
        active: bool,
        usage: MeshQuotaUsage,
    }

    #[typeshare]
    #[derive(Serialize)]
    struct MeshPeerOutgoingAccess {
        #[typeshare(serialized_as = "String")]
        revision: Uuid,
        grant: PeerGrant,
        quotas: Vec<MeshPeerQuota>,
    }

    #[typeshare]
    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum MeshPeerAccessMode {
        Full,
        Custom,
    }

    #[typeshare]
    #[derive(Serialize, Deserialize)]
    struct MeshPeerEffectiveAccess {
        mode: MeshPeerAccessMode,
        sessions: bool,
        filesystem: bool,
        shell: bool,
        providers: Vec<ProviderGrant>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    enum MeshPeerIncomingAccess {
        Granted {
            effective: MeshPeerEffectiveAccess,
            quotas: Vec<MeshPeerQuota>,
        },
        Denied,
        Legacy,
        Unavailable,
        Incompatible,
    }

    #[typeshare]
    #[derive(Serialize)]
    struct MeshShareableModel {
        model_id: String,
        finite_quota_supported: bool,
    }

    #[typeshare]
    #[derive(Serialize)]
    struct MeshShareableProvider {
        provider_id: String,
        models: Vec<MeshShareableModel>,
    }

    #[typeshare]
    #[derive(Serialize)]
    struct MeshPeerAccessResponse {
        version: u32,
        peer_id: String,
        outgoing: MeshPeerOutgoingAccess,
        #[serde(skip_serializing_if = "Option::is_none")]
        incoming: Option<MeshPeerIncomingAccess>,
        #[serde(skip_serializing_if = "Option::is_none")]
        shareable_catalog: Option<Vec<MeshShareableProvider>>,
    }

    enum Edit {
        Grant(Uuid, PeerGrant),
        Quota(Uuid, QuotaScope, QuotaEdit),
    }

    fn invalid() -> Error {
        Error::invalid_params().data(json!({"code": "mesh_access_invalid_params"}))
    }

    fn domain(code: &str, message: &str) -> Error {
        Error::new(-32000, message.to_owned()).data(json!({"code": code}))
    }

    pub(super) fn grant_error(error: GrantStoreError) -> Error {
        match error {
            GrantStoreError::Invalid(_) => invalid(),
            GrantStoreError::Conflict { expected, actual } => Error::new(-32000, "grant revision changed").data(json!({
                "code": "mesh_access_conflict", "expected_revision": expected, "actual_revision": actual,
            })),
            GrantStoreError::Backend(_) => domain("mesh_access_unavailable", "mesh access store unavailable"),
        }
    }

    fn quota_error(error: QuotaError, scope: Option<&QuotaScope>) -> Error {
        match error {
            QuotaError::Store(error) => grant_error(error),
            QuotaError::InvalidScope | QuotaError::InvalidTokens => invalid(),
            QuotaError::ReconfigureRequired => domain(
                "mesh_quota_reconfigure_required",
                "quota requires reconfiguration",
            ),
            QuotaError::InFlight => Error::new(-32000, "quota has outstanding attempts")
                .data(json!({"code": "mesh_quota_in_flight", "scope": scope})),
            QuotaError::Denied | QuotaError::NotFinite => {
                domain("mesh_access_denied", "quota operation denied")
            }
            _ => domain("mesh_access_unavailable", "quota state unavailable"),
        }
    }

    fn parse<T: DeserializeOwned>(req: &ExtRequest) -> Result<T, Error> {
        serde_json::from_str(req.params.get()).map_err(|_| invalid())
    }

    fn timestamp(seconds: i64) -> Option<String> {
        time::OffsetDateTime::from_unix_timestamp(seconds)
            .ok()?
            .format(&time::format_description::well_known::Rfc3339)
            .ok()
    }

    fn quota_wire(grant: &PeerGrant, quota: QuotaStatus) -> MeshPeerQuota {
        let finite = matches!(quota.limit, QuotaLimit::Finite { .. });
        let usage = MeshQuotaUsage {
            known_tokens: quota.known_tokens.to_string(),
            estimated_tokens: quota.estimated_tokens.to_string(),
            has_unknown_usage: quota.has_unknown_usage,
            reserved_tokens: finite.then(|| quota.reserved_tokens.to_string()),
            available_tokens: quota.available_tokens.map(|count| count.to_string()),
            window_started_at: quota.window_started_at.and_then(timestamp),
            next_reset_at: quota.next_reset_at.and_then(timestamp),
        };
        let limit = match quota.limit {
            QuotaLimit::Unlimited => MeshQuotaLimit::Unlimited,
            QuotaLimit::Finite { tokens, window } => MeshQuotaLimit::Finite {
                limit_tokens: tokens.to_string(),
                window,
            },
        };
        let active = match grant {
            PeerGrant::Custom { providers, .. } => {
                providers
                    .iter()
                    .any(|p| p.provider_id == quota.scope.provider_id)
                    && quota.scope.model_id.as_ref().is_none_or(|model| {
                        grant.allows_model(SharingProtocol::V1, &quota.scope.provider_id, model)
                    })
            }
            _ => false,
        };
        MeshPeerQuota {
            scope: quota.scope,
            limit,
            active,
            usage,
        }
    }

    type Catalog = std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;

    fn local_catalog(inventory: &crate::model_inventory::ModelInventory) -> Catalog {
        let mut catalog = Catalog::new();
        for entry in inventory.local_snapshot_entries_blocking() {
            catalog
                .entry(entry.provider)
                .or_default()
                .insert(entry.model);
        }
        catalog
    }

    fn peer_catalog_changed(grant: &PeerGrant, previous: &Catalog, current: &Catalog) -> bool {
        previous.iter().chain(current).any(|(provider, _)| {
            let before = previous.get(provider);
            let after = current.get(provider);
            before
                .into_iter()
                .flatten()
                .chain(after.into_iter().flatten())
                .any(|model| {
                    grant.allows_model(SharingProtocol::V1, provider, model)
                        && before.is_some_and(|models| models.contains(model))
                            != after.is_some_and(|models| models.contains(model))
                })
        })
    }

    #[cfg(test)]
    #[test]
    fn typed_peer_access_preserves_the_flat_wire_and_exact_tokens() {
        let quota = MeshPeerQuota {
            scope: QuotaScope {
                provider_id: "fixture".into(),
                model_id: None,
            },
            limit: MeshQuotaLimit::Finite {
                limit_tokens: "9007199254740993".into(),
                window: QuotaWindow::Daily,
            },
            active: true,
            usage: MeshQuotaUsage {
                known_tokens: "1".into(),
                estimated_tokens: "0".into(),
                has_unknown_usage: false,
                reserved_tokens: Some("0".into()),
                available_tokens: Some("9007199254740992".into()),
                window_started_at: Some("2024-02-29T00:00:00Z".into()),
                next_reset_at: None,
            },
        };
        let incoming = MeshPeerIncomingAccess::Granted {
            effective: MeshPeerEffectiveAccess {
                mode: MeshPeerAccessMode::Custom,
                sessions: true,
                filesystem: false,
                shell: false,
                providers: vec![],
            },
            quotas: vec![quota],
        };
        // An earlier v1 peer used an untyped named map for the same reply.
        let wire = rmp_serde::to_vec_named(&incoming).unwrap();
        let legacy_reader: serde_json::Value = rmp_serde::from_slice(&wire).unwrap();
        assert_eq!(legacy_reader["status"], "granted");
        assert_eq!(legacy_reader["effective"]["mode"], "custom");
        assert_eq!(
            legacy_reader["quotas"][0]["limit"]["limit_tokens"],
            "9007199254740993"
        );
        assert!(
            legacy_reader["quotas"][0]["usage"]
                .get("next_reset_at")
                .is_none()
        );
        let legacy_writer = rmp_serde::to_vec_named(&legacy_reader).unwrap();
        let typed_reader: MeshPeerIncomingAccess = rmp_serde::from_slice(&legacy_writer).unwrap();
        assert_eq!(serde_json::to_value(typed_reader).unwrap(), legacy_reader);
    }

    #[cfg(test)]
    #[test]
    fn catalog_hints_only_disclose_changes_visible_to_the_recipient() {
        use querymt_remote::peer_access::{ModelSelection, ProviderGrant};
        let grant = PeerGrant::Custom {
            sessions: false,
            filesystem: false,
            providers: vec![ProviderGrant {
                provider_id: "fixture".into(),
                models: ModelSelection::Selected {
                    model_ids: vec!["allowed".into()],
                },
            }],
        };
        let old = Catalog::from([("fixture".into(), ["allowed".into()].into())]);
        let hidden =
            Catalog::from([("fixture".into(), ["allowed".into(), "hidden".into()].into())]);
        assert!(!peer_catalog_changed(&grant, &old, &hidden));
        assert!(!peer_catalog_changed(
            &PeerGrant::None,
            &old,
            &Catalog::new()
        ));
        assert!(peer_catalog_changed(&grant, &old, &Catalog::new()));
        assert!(peer_catalog_changed(&PeerGrant::Full, &old, &hidden));
    }

    pub(crate) fn register_peer_access_read(
        router: &querymt_remote::mesh_ingress::InboundRouter,
        store: Arc<dyn querymt_remote::peer_access::quota::QuotaStore>,
        actor: &kameo::actor::ActorRef<crate::agent::remote::RemoteNodeManager>,
        handle: &LocalAgentHandle,
        mesh: &querymt_remote::MeshHandle,
    ) {
        use crate::agent::remote::node_manager::{GetPeerSharing, PeerAccessInvalidated};
        use crate::error::AgentError;
        use querymt_remote::mesh_ingress::{InboundAccess, PeerRequest};

        let notifications = handle.ext_notifications.clone();
        let inventory = handle.model_inventory.clone();
        let mut catalog_updates = inventory.subscribe_updates();
        let mut previous_catalog = local_catalog(&inventory);
        tokio::spawn({
            let actor = actor.downgrade();
            let notifications = notifications.clone();
            let store = store.clone();
            let mesh = mesh.clone();
            let inventory = inventory.clone();
            async move {
                let stopped = actor.wait_for_shutdown();
                tokio::pin!(stopped);
                let host = NodeId::from_peer_id(*mesh.peer_id());
                loop {
                    tokio::select! {
                        () = &mut stopped => break,
                        update = catalog_updates.recv() => if matches!(update, Err(tokio::sync::broadcast::error::RecvError::Closed)) { break; },
                    }
                    let current = local_catalog(&inventory);
                    if current == previous_catalog {
                        continue;
                    }
                    let previous = std::mem::replace(&mut previous_catalog, current);
                    let Ok(peers) = store.managed_peers(&host).await else {
                        continue;
                    };
                    for peer in peers {
                        notify(
                            &notifications,
                            &peer,
                            "outgoing",
                            PeerAccessInvalidated::Catalog,
                        );
                        let key = PeerGrantKey {
                            host: host.clone(),
                            peer: peer.clone(),
                        };
                        if let Ok(state) = store.grant(&key).await
                            && peer_catalog_changed(&state.grant, &previous, &previous_catalog)
                        {
                            notify_peer(mesh.clone(), peer, PeerAccessInvalidated::Catalog);
                        }
                    }
                }
            }
        });
        if let Some(mut usage) = store.subscribe_usage_changes() {
            let mesh = mesh.clone();
            let store = store.clone();
            let notifications = notifications.clone();
            let actor = actor.downgrade();
            tokio::spawn(async move {
                let stopped = actor.wait_for_shutdown();
                tokio::pin!(stopped);
                loop {
                    let key = tokio::select! {
                        () = &mut stopped => break,
                        key = usage.recv() => match key {
                            Ok(key) => key,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        },
                    };
                    if key.host.as_peer_id() == mesh.peer_id() {
                        notify(
                            &notifications,
                            &key.peer,
                            "outgoing",
                            PeerAccessInvalidated::Quota,
                        );
                        if store
                            .grant(&key)
                            .await
                            .is_ok_and(|state| matches!(state.grant, PeerGrant::Custom { .. }))
                        {
                            notify_peer(mesh.clone(), key.peer, PeerAccessInvalidated::Quota);
                        }
                    }
                }
            });
        }
        router.register_v1_ask::<crate::agent::remote::RemoteNodeManager, PeerAccessInvalidated, _, _, _, _>(
            actor.id(), InboundAccess::PeerAccess, move |peer: PeerRequest, reason: PeerAccessInvalidated| {
                let notifications = notifications.clone();
                async move {
                    notify(&notifications, &peer.key().peer, "incoming", reason);
                    Ok::<_, AgentError>(())
                }
            },
        );

        let inventory = handle.model_inventory.clone();
        router
            .register_v1_ask::<crate::agent::remote::RemoteNodeManager, GetPeerSharing, _, _, _, _>(
                actor.id(),
                InboundAccess::PeerAccess,
                move |peer: PeerRequest, _| {
                    let store = store.clone();
                    let inventory = inventory.clone();
                    async move {
                        let now = time::OffsetDateTime::now_utc().unix_timestamp();
                        let (state, quotas) = store
                            .access_snapshot(peer.key(), now)
                            .await
                            .map_err(|_| AgentError::MeshNotBootstrapped)?;
                        if peer.revision() != Some(state.revision) {
                            return Err(AgentError::MeshAccessDenied);
                        }
                        let cargo_catalog = local_catalog(&inventory);
                        let effective = match &state.grant {
                            PeerGrant::None => return Ok(MeshPeerIncomingAccess::Denied),
                            PeerGrant::Full => MeshPeerEffectiveAccess {
                                mode: MeshPeerAccessMode::Full,
                                sessions: true,
                                filesystem: true,
                                shell: true,
                                providers: cargo_catalog
                                    .into_keys()
                                    .map(|provider_id| ProviderGrant {
                                        provider_id,
                                        models: ModelSelection::All,
                                    })
                                    .collect(),
                            },
                            PeerGrant::Custom {
                                sessions,
                                filesystem,
                                providers,
                            } => MeshPeerEffectiveAccess {
                                mode: MeshPeerAccessMode::Custom,
                                sessions: *sessions,
                                filesystem: *filesystem,
                                shell: false,
                                providers: providers.clone(),
                            },
                        };
                        let quotas = quotas
                            .into_iter()
                            .map(|quota| quota_wire(&state.grant, quota))
                            .filter(|quota| quota.active)
                            .collect();
                        Ok(MeshPeerIncomingAccess::Granted { effective, quotas })
                    }
                },
            );
    }

    fn notify(
        sender: &tokio::sync::broadcast::Sender<ExtNotification>,
        peer: &NodeId,
        direction: &'static str,
        reason: crate::agent::remote::node_manager::PeerAccessInvalidated,
    ) {
        let params = json!({"version": 1, "peer_id": peer.to_string(), "direction": direction, "reason": reason.as_str()});
        if let Ok(params) = serde_json::value::to_raw_value(&params) {
            let _ = sender.send(ExtNotification::new(
                crate::acp::shared::QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED,
                Arc::from(params),
            ));
        }
    }

    fn notify_peer(
        mesh: querymt_remote::MeshHandle,
        peer: NodeId,
        reason: crate::agent::remote::node_manager::PeerAccessInvalidated,
    ) {
        use crate::agent::remote::{
            MeshRuntimeHandle, RemoteNodeManager, scope::scoped_node_manager_for_peer,
        };
        use querymt_remote::request_timeout::ask_sharing_v1;
        tokio::spawn(async move {
            let timeout = std::time::Duration::from_secs(3);
            let _ = tokio::time::timeout(timeout, async {
                let runtime = MeshRuntimeHandle::from(mesh.clone());
                for scope in runtime.active_scopes() {
                    let name = scoped_node_manager_for_peer(&scope, peer.as_peer_id());
                    if let Ok(Some(manager)) = runtime
                        .lookup_actor_no_retry::<RemoteNodeManager>(name)
                        .await
                        && manager.id().peer_id() == Some(peer.as_peer_id())
                    {
                        let _ = ask_sharing_v1::<_, _, (), crate::error::AgentError>(
                            &mesh, &manager, &reason, timeout,
                        )
                        .await;
                        break;
                    }
                }
            })
            .await;
        });
    }

    async fn incoming(
        agent: &LocalAgentHandle,
        mesh: &querymt_remote::MeshHandle,
        peer: &NodeId,
    ) -> MeshPeerIncomingAccess {
        use crate::agent::remote::node_manager::GetPeerSharing;
        use crate::error::AgentError;
        use kameo::error::RemoteSendError;
        use querymt_remote::request_timeout::{SharingAskError, ask_sharing_v1};

        let timeout = LocalAgentHandle::remote_node_info_timeout();
        let lookup = async {
            let protocol = mesh
                .select_sharing_protocol(*peer.as_peer_id(), timeout)
                .await
                .ok()?;
            if protocol == SharingProtocol::Legacy {
                return Some(MeshPeerIncomingAccess::Legacy);
            }
            let reference = agent.find_node_manager(&peer.to_string()).await.ok()?;
            if reference.id().peer_id() != Some(peer.as_peer_id()) {
                return None;
            }
            match ask_sharing_v1::<_, _, MeshPeerIncomingAccess, AgentError>(
                mesh,
                &reference,
                &GetPeerSharing,
                timeout,
            )
            .await
            {
                Ok(value) => Some(value),
                Err(SharingAskError::Actor(RemoteSendError::UnknownMessage { .. })) => {
                    Some(MeshPeerIncomingAccess::Incompatible)
                }
                Err(_) => None,
            }
        };
        tokio::time::timeout(timeout, lookup)
            .await
            .ok()
            .flatten()
            .unwrap_or(MeshPeerIncomingAccess::Unavailable)
    }

    async fn finite_quota_supported(
        provider: &crate::session::provider::SessionProvider,
        id: &str,
        model: &str,
    ) -> bool {
        use crate::agent::remote::provider_host_backend::qualified_text_stream_limit;
        use crate::session::provider_config::{ProviderConfigMode, resolve_provider_config};
        let discovery = async {
            let binding = provider.provider_resolver().resolve(id).await.ok()?;
            let defaults = querymt_remote::params_for_remote_provider(provider.initial_params());
            let resolved = resolve_provider_config(
                &binding,
                provider.initial_params(),
                ProviderConfigMode::Runtime {
                    model,
                    params: defaults.as_ref(),
                    api_key_override: None,
                    session_id: "standalone",
                },
            )
            .await
            .ok()?;
            qualified_text_stream_limit(
                &binding.factory.config_schema(),
                &resolved.pruned_config_str,
            )
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), discovery)
            .await
            .ok()
            .flatten()
            .is_some()
    }

    pub(super) async fn dispatch(
        agent: &LocalAgentHandle,
        req: ExtRequest,
    ) -> Result<ExtResponse, Error> {
        let (version, peer_id, edit) = match req.method.as_ref() {
            "querymt/mesh/getPeerAccess" => {
                let GetMeshPeerAccessRequest { version, peer_id } = parse(&req)?;
                (version, peer_id, None)
            }
            "querymt/mesh/setPeerAccess" => {
                let SetMeshPeerAccessRequest {
                    version,
                    peer_id,
                    expected_revision,
                    grant,
                } = parse(&req)?;
                (
                    version,
                    peer_id,
                    Some(Edit::Grant(expected_revision, grant)),
                )
            }
            "querymt/mesh/setPeerQuota" => {
                let SetMeshPeerQuotaRequest {
                    version,
                    peer_id,
                    expected_revision,
                    scope,
                    operation,
                    limit,
                } = parse(&req)?;
                let limit = match limit {
                    MeshQuotaLimit::Unlimited => QuotaLimit::Unlimited,
                    MeshQuotaLimit::Finite {
                        limit_tokens,
                        window,
                    } => {
                        if !limit_tokens.bytes().all(|b| b.is_ascii_digit()) {
                            return Err(invalid());
                        }
                        QuotaLimit::Finite {
                            tokens: limit_tokens.parse().map_err(|_| invalid())?,
                            window,
                        }
                    }
                };
                let edit = match operation {
                    MeshQuotaOperation::Update => QuotaEdit::Update(limit),
                    MeshQuotaOperation::Reconfigure => QuotaEdit::Reconfigure(limit),
                };
                (
                    version,
                    peer_id,
                    Some(Edit::Quota(expected_revision, scope, edit)),
                )
            }
            "querymt/mesh/resetPeerQuota" => {
                let ResetMeshPeerQuotaRequest {
                    version,
                    peer_id,
                    expected_revision,
                    scope,
                } = parse(&req)?;
                (
                    version,
                    peer_id,
                    Some(Edit::Quota(expected_revision, scope, QuotaEdit::Reset)),
                )
            }
            _ => return Err(Error::method_not_found()),
        };
        if version != 1 {
            return Err(invalid());
        }
        let peer = NodeId::parse(&peer_id).map_err(|_| invalid())?;
        let unavailable = || {
            domain(
                "mesh_access_unavailable",
                "local mesh access store unavailable",
            )
        };
        let mesh = agent.mesh().ok_or_else(unavailable)?;
        let store = mesh.quota_store().ok_or_else(unavailable)?;
        let host = NodeId::from_peer_id(*mesh.peer_id());
        if host == peer {
            return Err(invalid());
        }
        let key = PeerGrantKey { host, peer };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let change = match edit {
            Some(Edit::Grant(expected, grant)) => {
                let state = store
                    .replace_grant(&key, expected, grant)
                    .await
                    .map_err(grant_error)?;
                (state.revision != expected)
                    .then_some(crate::agent::remote::node_manager::PeerAccessInvalidated::Policy)
            }
            Some(Edit::Quota(expected, scope, edit)) => {
                let revision = store
                    .edit_quota(&key, expected, &scope, edit, now)
                    .await
                    .map_err(|error| quota_error(error, Some(&scope)))?;
                (revision != expected)
                    .then_some(crate::agent::remote::node_manager::PeerAccessInvalidated::Quota)
            }
            None => None,
        };
        if let Some(reason) = change {
            notify(&agent.ext_notifications, &key.peer, "outgoing", reason);
            notify_peer(mesh.clone(), key.peer.clone(), reason);
        }
        let (state, statuses) = store
            .access_snapshot(&key, now)
            .await
            .map_err(|error| quota_error(error, None))?;
        let quotas: Vec<_> = statuses
            .into_iter()
            .map(|quota| quota_wire(&state.grant, quota))
            .collect();
        let mut response = MeshPeerAccessResponse {
            version: 1,
            peer_id,
            outgoing: MeshPeerOutgoingAccess {
                revision: state.revision,
                grant: state.grant,
                quotas,
            },
            incoming: None,
            shareable_catalog: None,
        };
        if req.method.as_ref() == "querymt/mesh/getPeerAccess" {
            let mut catalog = Vec::new();
            for (provider_id, models) in local_catalog(&agent.model_inventory) {
                let mut rows = Vec::with_capacity(models.len());
                for model_id in models {
                    let supported = if provider_id == "llama-cpp" {
                        finite_quota_supported(&agent.config.provider, &provider_id, &model_id)
                            .await
                    } else {
                        false
                    };
                    rows.push(MeshShareableModel {
                        model_id,
                        finite_quota_supported: supported,
                    });
                }
                catalog.push(MeshShareableProvider {
                    provider_id,
                    models: rows,
                });
            }
            response.shareable_catalog = Some(catalog);
            response.incoming = Some(incoming(agent, &mesh, &key.peer).await);
        }
        ext_json_response(&response)
    }
}
