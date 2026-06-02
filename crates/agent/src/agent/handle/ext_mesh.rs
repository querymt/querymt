use super::*;
use super::utils::ext_json_response;

impl LocalAgentHandle {
    pub(super) async fn handle_ext_mesh_status(&self) -> Result<ExtResponse, Error> {
        #[cfg(feature = "remote")]
        {
            if let Some(mesh) = self.mesh() {
                return ext_json_response(&serde_json::json!({
                    "enabled": true,
                    "peer_id": mesh.peer_id().to_string(),
                    "transport": if mesh.is_iroh_transport_internal() { "iroh" } else { "lan" },
                    "known_peer_count": mesh.known_peer_ids().len(),
                    "has_invite_store": mesh.invite_store().is_some(),
                    "has_mesh_state_store": mesh.mesh_state_store().is_some(),
                }));
            }
        }

        ext_json_response(&serde_json::json!({
            "enabled": false,
            "peer_id": serde_json::Value::Null,
            "transport": serde_json::Value::Null,
            "known_peer_count": 0,
            "has_invite_store": false,
            "has_membership_store": false,
        }))
    }

    pub(super) async fn handle_ext_mesh_join(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[cfg(feature = "remote")]
        {
            #[derive(serde::Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct JoinReq {
                invite: String,
            }

            let parsed: JoinReq = serde_json::from_str(req.params.get()).map_err(|e| {
                Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
            })?;

            let invite = crate::agent::remote::invite::SignedInviteGrant::decode(&parsed.invite)
                .map_err(|e| {
                    Error::invalid_params()
                        .data(serde_json::json!({"error": format!("invalid mesh invite: {}", e)}))
                })?;

            let mesh = self.mesh().ok_or_else(|| {
                Error::internal_error().data(serde_json::json!({ "error": "mesh not bootstrapped" }))
            })?;
            let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
            let mesh_id = crate::agent::remote::invite::mesh_id_for(
                &invite.grant.inviter_peer_id,
                invite.grant.mesh_name.as_deref(),
            );
            let already_joined = runtime.joined_iroh_scopes().into_iter().any(|scope| {
                matches!(scope, crate::agent::remote::MeshScopeId::Iroh { mesh_id: ref existing } if existing == &mesh_id)
            });

            if !already_joined {
                self.join_mesh_invite(invite.clone()).await.map_err(|e| {
                    Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
                })?;
            }

            return ext_json_response(&serde_json::json!({
                "joined": true,
                "peer_id": runtime.peer_id().to_string(),
                "mesh_id": mesh_id,
                "mesh_name": invite.grant.mesh_name,
                "inviter_peer_id": invite.grant.inviter_peer_id,
                "already_joined": already_joined,
            }));
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = req;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_mesh_nodes(&self) -> Result<ExtResponse, Error> {
        #[cfg(feature = "remote")]
        {
            let nodes = self
                .list_remote_nodes()
                .await
                .into_iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.node_id.to_string(),
                        "label": n.hostname,
                        "capabilities": n.capabilities,
                        "active_sessions": n.active_sessions,
                    })
                })
                .collect::<Vec<_>>();
            return ext_json_response(&serde_json::json!({ "nodes": nodes }));
        }

        #[cfg(not(feature = "remote"))]
        {
            ext_json_response(&serde_json::json!({ "nodes": [] }))
        }
    }

    pub(super) async fn handle_ext_mesh_create_invite(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize, Default)]
        struct CreateInviteReq {
            #[serde(default)]
            #[serde(alias = "meshName")]
            mesh_name: Option<String>,
            #[serde(default)]
            ttl: Option<String>,
            #[serde(default)]
            #[serde(alias = "maxUses")]
            max_uses: Option<u32>,
        }

        let parsed: CreateInviteReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let Some(mesh) = self.mesh() else {
                return Err(Error::invalid_request().data(
                    serde_json::json!({"error": "mesh not bootstrapped - start with --mesh"}),
                ));
            };

            if !mesh.is_iroh_transport_internal() {
                return ext_json_response(&serde_json::json!({
                    "error": "mesh invites require iroh transport; restart host with --mesh --mesh-invite (or set transport=iroh)"
                }));
            }

            let ttl_secs = parsed
                .ttl
                .as_deref()
                .and_then(crate::agent::remote::invite::parse_duration_secs)
                .or(Some(24 * 3600));

            let invite = mesh
                .create_invite(parsed.mesh_name.clone(), ttl_secs, parsed.max_uses, false)
                .map_err(|e| {
                    Error::internal_error().data(serde_json::json!({"error": format!("{e}")}))
                })?;
            let scope = crate::agent::remote::scope::MeshScopeId::Iroh {
                mesh_id: crate::agent::remote::invite::mesh_id_for(
                    &invite.grant.inviter_peer_id,
                    invite.grant.mesh_name.as_deref(),
                ),
            };
            self.publish_mesh_scope(
                &crate::agent::remote::MeshRuntimeHandle::from(mesh.clone()),
                &scope,
            )
            .await
            .map_err(|e| {
                Error::internal_error().data(serde_json::json!({"error": format!("{e}")}))
            })?;

            let qr_code = crate::agent::remote::qr::render_to_terminal(&invite.to_url());

            return ext_json_response(&serde_json::json!({
                "inviteId": invite.grant.invite_id,
                "url": invite.to_url(),
                "qrCode": qr_code,
                "expiresAt": invite.grant.expires_at,
                "maxUses": invite.grant.max_uses,
                "meshName": parsed.mesh_name,
            }));
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_mesh_list_invites(&self) -> Result<ExtResponse, Error> {
        #[cfg(feature = "remote")]
        {
            let Some(mesh) = self.mesh() else {
                return ext_json_response(&serde_json::json!({"invites": []}));
            };

            let invites: Vec<serde_json::Value> = if let Some(store) = mesh.invite_store() {
                let store = store.read();
                store
                    .list_pending()
                    .into_iter()
                    .map(|r| {
                        serde_json::json!({
                            "inviteId": r.invite_id,
                            "meshName": r.grant.mesh_name,
                            "expiresAt": r.grant.expires_at,
                            "maxUses": r.grant.max_uses,
                            "usesRemaining": r.uses_remaining,
                            "status": match r.status {
                                crate::agent::remote::invite::InviteStatus::Pending => "pending",
                                crate::agent::remote::invite::InviteStatus::Consumed => "consumed",
                                crate::agent::remote::invite::InviteStatus::Revoked => "revoked",
                            },
                            "usedBy": r.used_by,
                            "createdAt": r.created_at,
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };

            return ext_json_response(&serde_json::json!({"invites": invites}));
        }

        #[cfg(not(feature = "remote"))]
        {
            Err(Error::method_not_found())
        }
    }

    pub(super) async fn handle_ext_mesh_revoke_invite(&self, req: ExtRequest) -> Result<ExtResponse, Error> {
        #[cfg_attr(not(feature = "remote"), allow(dead_code))]
        #[derive(serde::Deserialize)]
        struct RevokeInviteReq {
            #[serde(alias = "inviteId")]
            invite_id: String,
        }

        let parsed: RevokeInviteReq = serde_json::from_str(req.params.get()).map_err(|e| {
            Error::invalid_params().data(serde_json::json!({"error": e.to_string()}))
        })?;

        #[cfg(feature = "remote")]
        {
            let Some(mesh) = self.mesh() else {
                return ext_json_response(&serde_json::json!({
                    "success": false,
                    "message": "mesh not bootstrapped - start with --mesh"
                }));
            };

            let result = if let Some(store) = mesh.invite_store() {
                store.write().revoke(&parsed.invite_id)
            } else {
                Err(crate::agent::remote::invite::InviteError::StoreError(
                    "invite store not available".to_string(),
                ))
            };

            return match result {
                Ok(()) => ext_json_response(&serde_json::json!({
                    "success": true,
                    "message": null,
                })),
                Err(e) => ext_json_response(&serde_json::json!({
                    "success": false,
                    "message": e.to_string(),
                })),
            };
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = parsed;
            Err(Error::method_not_found())
        }
    }
}
