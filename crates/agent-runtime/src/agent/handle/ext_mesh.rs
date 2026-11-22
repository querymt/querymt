use super::utils::ext_json_response;
use super::*;

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

    pub(super) async fn handle_ext_mesh_nodes(&self) -> Result<ExtResponse, Error> {
        ext_json_response(&crate::control::mesh::list_nodes(self).await)
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
    use crate::agent::remote::{RemoteNodeManager, node_manager::GetPeerSharing};
    use crate::error::AgentError;
    use querymt_remote::{
        mesh_ingress::{InboundAccess, InboundDelivery, InboundRouter, PeerRequest},
        peer_access::{
            ModelSelection, PeerGrant, ProviderGrant, SharingProtocol,
            quota::{QuotaLimit, QuotaScope, QuotaStatus, QuotaStore, QuotaWindow},
        },
    };
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum MeshQuotaLimit {
        Unlimited,
        Finite {
            limit_tokens: String,
            #[serde(default)]
            window: QuotaWindow,
        },
    }

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

    #[derive(Serialize, Deserialize)]
    struct MeshPeerQuota {
        scope: QuotaScope,
        limit: MeshQuotaLimit,
        active: bool,
        usage: MeshQuotaUsage,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum MeshPeerAccessMode {
        Full,
        Custom,
    }

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

    pub(crate) fn register_peer_access_read(
        router: &InboundRouter,
        store: Arc<dyn QuotaStore>,
        actor_id: kameo::actor::ActorId,
        handle: &LocalAgentHandle,
    ) {
        let inventory = handle.model_inventory.clone();
        // This read authenticates the caller but does not require a grant: the
        // handler reads only that caller's snapshot, including a Denied reply.
        router.register::<RemoteNodeManager, GetPeerSharing, _, _, _, _>(
            actor_id,
            InboundDelivery::V1Ask,
            InboundAccess::Identity,
            move |peer: PeerRequest, _| {
                let store = store.clone();
                let inventory = inventory.clone();
                async move {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    let (state, quotas) = store
                        .access_snapshot(peer.key(), now)
                        .await
                        .map_err(|_| AgentError::MeshNotBootstrapped)?;
                    let effective = match &state.grant {
                        PeerGrant::None => return Ok(MeshPeerIncomingAccess::Denied),
                        PeerGrant::Full => MeshPeerEffectiveAccess {
                            mode: MeshPeerAccessMode::Full,
                            sessions: true,
                            filesystem: true,
                            shell: true,
                            providers: inventory
                                .local_snapshot_entries_blocking()
                                .into_iter()
                                .map(|entry| entry.provider)
                                .collect::<std::collections::BTreeSet<_>>()
                                .into_iter()
                                .map(|provider_id| ProviderGrant {
                                    provider_id,
                                    models: ModelSelection::All,
                                })
                                .collect(),
                        },
                        PeerGrant::Custom {
                            sessions,
                            filesystem,
                            shell,
                            providers,
                        } => MeshPeerEffectiveAccess {
                            mode: MeshPeerAccessMode::Custom,
                            sessions: *sessions,
                            filesystem: *filesystem,
                            shell: *shell,
                            providers: providers.clone(),
                        },
                    };
                    let quotas = quotas
                        .into_iter()
                        .map(|quota| quota_wire(&state.grant, quota))
                        .filter(|quota| quota.active)
                        .collect();
                    Ok::<_, AgentError>(MeshPeerIncomingAccess::Granted { effective, quotas })
                }
            },
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::model_inventory::catalog_fixture::CatalogFixture;
        use kameo::actor::ActorId;
        use kameo::remote::{
            RemoteActor, RemoteMessage,
            messaging::{SwarmRequest, SwarmResponse},
        };
        use querymt_remote::{
            NodeId,
            mesh_ingress::MeshIngress,
            peer_access::{GrantStore, PeerGrantKey},
        };
        use serde_json::{Value, json};
        use std::time::Duration;
        use uuid::Uuid;

        #[test]
        fn typed_peer_access_preserves_the_flat_wire_and_exact_tokens() {
            let incoming = MeshPeerIncomingAccess::Granted {
                effective: MeshPeerEffectiveAccess {
                    mode: MeshPeerAccessMode::Custom,
                    sessions: true,
                    filesystem: false,
                    shell: false,
                    providers: vec![],
                },
                quotas: vec![MeshPeerQuota {
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
                }],
            };
            let wire = rmp_serde::to_vec_named(&incoming).unwrap();
            let legacy_reader: Value = rmp_serde::from_slice(&wire).unwrap();
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
            let typed_reader: MeshPeerIncomingAccess =
                rmp_serde::from_slice(&legacy_writer).unwrap();
            assert_eq!(serde_json::to_value(typed_reader).unwrap(), legacy_reader);
        }

        #[tokio::test]
        async fn catalog_withdrawal_refreshes_v1_peer_access_without_rewriting_grant() {
            let (config, fixture, _dir) = CatalogFixture::config().await;
            let handle = LocalAgentHandle::from_config(config);
            let inventory = &handle.model_inventory;
            let deadline = Duration::from_secs(5);
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(querymt_remote::MeshAccessStore::new(
                querymt_remote::MeshStateStore::shared(&dir.path().join("mesh_state.json")).unwrap(),
            ));
            let host = libp2p::PeerId::random();
            let peer = libp2p::PeerId::random();
            let key = PeerGrantKey {
                host: NodeId::from_peer_id(host),
                peer: NodeId::from_peer_id(peer),
            };
            let grant = store
                .replace_grant(&key, Uuid::nil(), PeerGrant::Full)
                .await
                .unwrap();
            let router = InboundRouter::new(host, store.clone());
            let actor_id = ActorId::new_with_peer_id(1, host);
            register_peer_access_read(&router, store.clone(), actor_id, &handle);
            let get = |peer| {
                let router = &router;
                async move {
                    let request = SwarmRequest::Ask {
                        actor_id,
                        actor_remote_id: <RemoteNodeManager as RemoteActor>::REMOTE_ID.into(),
                        message_remote_id:
                            <RemoteNodeManager as RemoteMessage<GetPeerSharing>>::REMOTE_ID.into(),
                        payload: rmp_serde::to_vec_named(&GetPeerSharing).unwrap(),
                        mailbox_timeout: None,
                        reply_timeout: None,
                        immediate: false,
                    };
                    let SwarmResponse::Ask(Ok(bytes)) =
                        router.handle_v1(peer, request).await.unwrap()
                    else {
                        panic!("expected a peer access snapshot");
                    };
                    rmp_serde::from_slice::<Value>(&bytes).unwrap()
                }
            };

            tokio::time::timeout(deadline, inventory.ensure_local_refresh().await.wait())
                .await
                .expect("initial local catalog refresh");
            let before = get(peer).await;
            assert_eq!(before["status"], "granted");
            assert_eq!(
                before["effective"]["providers"],
                json!([{"provider_id": "fixture", "models": {"mode": "all"}}])
            );
            assert_eq!(
                get(libp2p::PeerId::random()).await,
                json!({"status": "denied"})
            );

            fixture.set_models(&[]);
            inventory.invalidate_local().await;
            tokio::time::timeout(deadline, inventory.ensure_local_refresh().await.wait())
                .await
                .expect("local catalog withdrawal refresh");
            let after = get(peer).await;
            assert_eq!(after["status"], "granted");
            assert_eq!(after["effective"]["providers"], json!([]));
            assert_eq!(store.grant(&key).await.unwrap().revision, grant.revision);
        }
    }
}
