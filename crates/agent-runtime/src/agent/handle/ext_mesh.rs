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
        peer_access::{GrantStore, PeerGrant},
    };
    use serde_json::{Value, json};

    pub(crate) fn register_peer_access_read(
        router: &InboundRouter,
        store: Arc<dyn GrantStore>,
        actor_id: kameo::actor::ActorId,
    ) {
        // Read only the authenticated caller's grant, even when it is None.
        router.register::<RemoteNodeManager, GetPeerSharing, _, _, _, _>(
            actor_id,
            InboundDelivery::V1Ask,
            InboundAccess::Identity,
            move |peer: PeerRequest, _| {
                let store = store.clone();
                async move {
                    let state = store
                        .grant(peer.key())
                        .await
                        .map_err(|_| AgentError::MeshNotBootstrapped)?;
                    Ok::<_, AgentError>(session_access_snapshot(state.grant))
                }
            },
        );
    }

    fn session_access_snapshot(grant: PeerGrant) -> Value {
        let (mode, sessions, filesystem, shell) = match grant {
            PeerGrant::None => return json!({"status": "denied"}),
            PeerGrant::Full => ("full", true, true, true),
            PeerGrant::Custom {
                sessions,
                filesystem,
                shell,
                ..
            } => ("custom", sessions, filesystem, shell),
        };
        // Keep the peer reply shape without advertising unavailable services.
        json!({
            "status": "granted",
            "effective": {
                "mode": mode,
                "sessions": sessions,
                "filesystem": filesystem,
                "shell": shell,
                "providers": [],
            },
            "quotas": [],
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use kameo::actor::ActorId;
        use kameo::remote::{
            RemoteActor, RemoteMessage,
            messaging::{SwarmRequest, SwarmResponse},
        };
        use querymt_remote::{
            NodeId,
            mesh_ingress::MeshIngress,
            peer_access::{ModelSelection, PeerGrantKey, ProviderGrant},
        };
        use uuid::Uuid;

        #[tokio::test]
        async fn peer_access_read_is_caller_scoped_and_omits_unavailable_providers() {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(querymt_remote::MeshAccessStore::new(
                querymt_remote::MeshStateStore::shared(&dir.path().join("mesh_state.json"))
                    .unwrap(),
            ));
            let host = libp2p::PeerId::random();
            let peer = libp2p::PeerId::random();
            let key = PeerGrantKey {
                host: NodeId::from_peer_id(host),
                peer: NodeId::from_peer_id(peer),
            };
            let router = InboundRouter::new(host, store.clone());
            let actor_id = ActorId::new_with_peer_id(1, host);
            register_peer_access_read(&router, store.clone(), actor_id);
            let request = || SwarmRequest::Ask {
                actor_id,
                actor_remote_id: <RemoteNodeManager as RemoteActor>::REMOTE_ID.into(),
                message_remote_id: <RemoteNodeManager as RemoteMessage<GetPeerSharing>>::REMOTE_ID
                    .into(),
                payload: rmp_serde::to_vec_named(&GetPeerSharing).unwrap(),
                mailbox_timeout: None,
                reply_timeout: None,
                immediate: false,
            };
            let get = |peer| {
                let router = &router;
                let request = request();
                async move {
                    let SwarmResponse::Ask(Ok(bytes)) =
                        router.handle_v1(peer, request).await.unwrap()
                    else {
                        panic!("expected a peer access snapshot");
                    };
                    rmp_serde::from_slice::<Value>(&bytes).unwrap()
                }
            };

            assert_eq!(get(peer).await, json!({"status": "denied"}));
            let granted = store
                .replace_grant(
                    &key,
                    Uuid::nil(),
                    PeerGrant::Custom {
                        sessions: true,
                        filesystem: false,
                        shell: false,
                        providers: vec![ProviderGrant {
                            provider_id: "fixture".into(),
                            models: ModelSelection::All,
                        }],
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                get(peer).await,
                json!({
                    "status": "granted",
                    "effective": {
                        "mode": "custom", "sessions": true, "filesystem": false,
                        "shell": false, "providers": [],
                    },
                    "quotas": [],
                })
            );
            assert_eq!(
                get(libp2p::PeerId::random()).await,
                json!({"status": "denied"})
            );
            let full = store
                .replace_grant(&key, granted.revision, PeerGrant::Full)
                .await
                .unwrap();
            assert_eq!(get(peer).await["effective"]["shell"], true);
            assert_eq!(get(peer).await["effective"]["providers"], json!([]));
            store
                .replace_grant(&key, full.revision, PeerGrant::None)
                .await
                .unwrap();
            assert_eq!(get(peer).await, json!({"status": "denied"}));
        }
    }
}
