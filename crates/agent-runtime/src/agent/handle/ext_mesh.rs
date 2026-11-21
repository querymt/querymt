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
            GrantStoreError, PeerGrant, PeerGrantKey, SharingProtocol,
            quota::{QuotaEdit, QuotaError, QuotaLimit, QuotaScope, QuotaStatus, QuotaWindow},
        },
    };
    use serde::{Deserialize, de::DeserializeOwned};
    use serde_json::{Value, json};
    use uuid::Uuid;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Read {
        version: u32,
        peer_id: String,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SetGrant {
        version: u32,
        peer_id: String,
        expected_revision: Uuid,
        grant: PeerGrant,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SetQuota {
        version: u32,
        peer_id: String,
        expected_revision: Uuid,
        scope: QuotaScope,
        operation: Operation,
        limit: WireLimit,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reset {
        version: u32,
        peer_id: String,
        expected_revision: Uuid,
        scope: QuotaScope,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Operation {
        Update,
        Reconfigure,
    }

    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum WireLimit {
        Unlimited,
        Finite {
            limit_tokens: String,
            #[serde(default)]
            window: QuotaWindow,
        },
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

    fn quota_json(grant: &PeerGrant, quota: QuotaStatus) -> Value {
        let mut usage = json!({ "known_tokens": quota.known_tokens.to_string(),
            "estimated_tokens": quota.estimated_tokens.to_string(), "has_unknown_usage": quota.has_unknown_usage });
        let limit = match quota.limit {
            QuotaLimit::Unlimited => json!({"kind": "unlimited"}),
            QuotaLimit::Finite { tokens, window } => {
                usage["reserved_tokens"] = json!(quota.reserved_tokens.to_string());
                usage["available_tokens"] =
                    json!(quota.available_tokens.map(|count| count.to_string()));
                usage["window_started_at"] = json!(quota.window_started_at.and_then(timestamp));
                if let Some(reset) = quota.next_reset_at.and_then(timestamp) {
                    usage["next_reset_at"] = json!(reset);
                }
                json!({"kind": "finite", "limit_tokens": tokens.to_string(), "window": window})
            }
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
        json!({"scope": quota.scope, "limit": limit, "active": active, "usage": usage})
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

    pub(crate) fn register_peer_access_read(
        router: &querymt_remote::mesh_ingress::InboundRouter,
        store: Arc<dyn querymt_remote::peer_access::quota::QuotaStore>,
        actor: &kameo::actor::ActorRef<crate::agent::remote::RemoteNodeManager>,
        inventory: crate::model_inventory::ModelInventory,
    ) {
        use crate::agent::remote::node_manager::GetPeerSharing;
        use crate::error::AgentError;
        use querymt_remote::mesh_ingress::{InboundAccess, PeerRequest};

        router.register_v1_ask::<crate::agent::remote::RemoteNodeManager, GetPeerSharing, _, _, _, _>(
            actor.id(), InboundAccess::PeerAccess, move |peer: PeerRequest, _| {
                let store = store.clone();
                let inventory = inventory.clone();
                async move {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    let (state, quotas) = store.access_snapshot(peer.key(), now).await
                        .map_err(|_| AgentError::MeshNotBootstrapped)?;
                    if peer.revision() != Some(state.revision) { return Err(AgentError::MeshAccessDenied); }
                    let cargo_catalog = local_catalog(&inventory);
                    let effective = match &state.grant {
                        PeerGrant::None => return Ok(json!({"status": "denied"})),
                        PeerGrant::Full => {
                            let providers: Vec<_> = cargo_catalog.keys().map(|provider_id| json!({"provider_id": provider_id, "models": {"mode": "all"}})).collect();
                            json!({"mode": "full", "sessions": true, "filesystem": true, "shell": true, "providers": providers})
                        }
                        PeerGrant::Custom { sessions, filesystem, providers } => json!({"mode": "custom", "sessions": sessions, "filesystem": filesystem, "shell": false, "providers": providers}),
                    };
                    let quotas: Vec<_> = quotas.into_iter().filter_map(|quota| {
                        let value = quota_json(&state.grant, quota);
                        (value["active"] == true).then_some(value)
                    }).collect();
                    Ok(json!({"status": "granted", "effective": effective, "quotas": quotas}))
                }
            },
        );
    }

    async fn incoming(
        agent: &LocalAgentHandle,
        mesh: &querymt_remote::MeshHandle,
        peer: &NodeId,
    ) -> Value {
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
                return Some(json!({"status": "legacy"}));
            }
            let reference = agent.find_node_manager(&peer.to_string()).await.ok()?;
            if reference.id().peer_id() != Some(peer.as_peer_id()) {
                return None;
            }
            match ask_sharing_v1::<_, _, Value, AgentError>(
                mesh,
                &reference,
                &GetPeerSharing,
                timeout,
            )
            .await
            {
                Ok(value) => Some(value),
                Err(SharingAskError::Actor(RemoteSendError::UnknownMessage { .. })) => {
                    Some(json!({"status": "incompatible"}))
                }
                Err(_) => None,
            }
        };
        tokio::time::timeout(timeout, lookup)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| json!({"status": "unavailable"}))
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
                let Read { version, peer_id } = parse(&req)?;
                (version, peer_id, None)
            }
            "querymt/mesh/setPeerAccess" => {
                let SetGrant {
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
                let SetQuota {
                    version,
                    peer_id,
                    expected_revision,
                    scope,
                    operation,
                    limit,
                } = parse(&req)?;
                let limit = match limit {
                    WireLimit::Unlimited => QuotaLimit::Unlimited,
                    WireLimit::Finite {
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
                    Operation::Update => QuotaEdit::Update(limit),
                    Operation::Reconfigure => QuotaEdit::Reconfigure(limit),
                };
                (
                    version,
                    peer_id,
                    Some(Edit::Quota(expected_revision, scope, edit)),
                )
            }
            "querymt/mesh/resetPeerQuota" => {
                let Reset {
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
                (state.revision != expected).then_some("policy")
            }
            Some(Edit::Quota(expected, scope, edit)) => {
                let revision = store
                    .edit_quota(&key, expected, &scope, edit, now)
                    .await
                    .map_err(|error| quota_error(error, Some(&scope)))?;
                (revision != expected).then_some("quota")
            }
            None => None,
        };
        if let Some(reason) = change {
            let params = serde_json::value::to_raw_value(&json!({"version": 1, "peer_id": peer_id, "direction": "outgoing", "reason": reason}))
                .map_err(Error::into_internal_error)?;
            agent.broadcast_ext_notification(ExtNotification::new(
                crate::acp::shared::QMT_NOTIFICATION_MESH_PEER_ACCESS_CHANGED,
                Arc::from(params),
            ));
        }
        let (state, statuses) = store
            .access_snapshot(&key, now)
            .await
            .map_err(|error| quota_error(error, None))?;
        let quotas: Vec<_> = statuses
            .into_iter()
            .map(|quota| quota_json(&state.grant, quota))
            .collect();
        let mut response = json!({ "version": 1, "peer_id": peer_id,
            "outgoing": {"revision": state.revision, "grant": state.grant, "quotas": quotas} });
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
                    rows.push(json!({"model_id": model_id, "finite_quota_supported": supported}));
                }
                catalog.push(json!({"provider_id": provider_id, "models": rows}));
            }
            response["shareable_catalog"] = json!(catalog);
            response["incoming"] = incoming(agent, &mesh, &key.peer).await;
        }
        ext_json_response(&response)
    }
}
