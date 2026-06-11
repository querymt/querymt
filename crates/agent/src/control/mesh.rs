use crate::LocalAgentHandle;
use agent_client_protocol::Error;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshScopeInfo {
    pub kind: String,
    pub id: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshStatusInfo {
    pub enabled: bool,
    pub peer_id: Option<String>,
    pub transport: Option<String>,
    pub known_peer_count: u32,
    pub has_invite_store: bool,
    pub has_mesh_state_store: bool,
    pub scopes: Vec<MeshScopeInfo>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshJoinRequest {
    pub invite: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshJoinInfo {
    pub joined: bool,
    pub peer_id: String,
    pub mesh_id: String,
    pub mesh_name: Option<String>,
    pub inviter_peer_id: String,
    pub already_joined: bool,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshNodesInfo {
    pub nodes: Vec<crate::control::remote::RemoteNodeInfo>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMeshInviteRequest {
    #[serde(default)]
    pub mesh_name: Option<String>,
    #[serde(default)]
    pub ttl: Option<String>,
    #[serde(default)]
    pub max_uses: Option<u32>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshInviteCreatedInfo {
    pub invite_id: String,
    pub url: String,
    pub qr_code: Option<String>,
    #[typeshare(serialized_as = "number")]
    pub expires_at: u64,
    #[typeshare(serialized_as = "number")]
    pub max_uses: u32,
    pub mesh_name: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshInviteInfo {
    pub invite_id: String,
    pub mesh_name: Option<String>,
    #[typeshare(serialized_as = "number")]
    pub expires_at: u64,
    #[typeshare(serialized_as = "number")]
    pub max_uses: u32,
    #[typeshare(serialized_as = "number")]
    pub uses_remaining: u32,
    pub status: String,
    pub used_by: Vec<String>,
    #[typeshare(serialized_as = "number")]
    pub created_at: u64,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshInviteListInfo {
    pub invites: Vec<MeshInviteInfo>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeMeshInviteRequest {
    pub invite_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshInviteRevokedInfo {
    pub success: bool,
    pub invite_id: String,
    pub message: Option<String>,
}

#[cfg(feature = "remote")]
fn map_invite_record(record: &crate::agent::remote::invite::InviteRecord) -> MeshInviteInfo {
    MeshInviteInfo {
        invite_id: record.invite_id.clone(),
        mesh_name: record.grant.mesh_name.clone(),
        expires_at: record.grant.expires_at,
        max_uses: record.grant.max_uses,
        uses_remaining: record.uses_remaining,
        status: match record.status {
            crate::agent::remote::invite::InviteStatus::Pending => "pending",
            crate::agent::remote::invite::InviteStatus::Consumed => "consumed",
            crate::agent::remote::invite::InviteStatus::Revoked => "revoked",
        }
        .to_string(),
        used_by: record.used_by.clone(),
        created_at: record.created_at,
    }
}

pub async fn status(agent: &LocalAgentHandle) -> MeshStatusInfo {
    #[cfg(feature = "remote")]
    {
        if let Some(mesh) = agent.mesh() {
            let transport = if mesh.is_iroh_transport_internal() {
                "iroh"
            } else {
                "lan"
            };
            let runtime = crate::agent::remote::MeshRuntimeHandle::from(mesh.clone());
            let scopes = runtime
                .active_scopes()
                .into_iter()
                .map(|scope| match scope {
                    crate::agent::remote::MeshScopeId::Lan { lan_id, .. } => MeshScopeInfo {
                        kind: "lan".to_string(),
                        id: lan_id,
                    },
                    crate::agent::remote::MeshScopeId::Iroh { mesh_id } => MeshScopeInfo {
                        kind: "iroh".to_string(),
                        id: mesh_id,
                    },
                })
                .collect();
            return MeshStatusInfo {
                enabled: true,
                peer_id: Some(mesh.peer_id().to_string()),
                transport: Some(transport.to_string()),
                known_peer_count: crate::agent::utils::u32_from_usize(
                    mesh.known_peer_ids().len(),
                    "known_peer_count",
                    None,
                ),
                has_invite_store: mesh.invite_store().is_some(),
                has_mesh_state_store: mesh.mesh_state_store().is_some(),
                scopes,
            };
        }
    }

    MeshStatusInfo {
        enabled: false,
        peer_id: None,
        transport: None,
        known_peer_count: 0,
        has_invite_store: false,
        has_mesh_state_store: false,
        scopes: Vec::new(),
    }
}

pub async fn join(
    agent: &LocalAgentHandle,
    request: MeshJoinRequest,
) -> Result<MeshJoinInfo, Error> {
    #[cfg(feature = "remote")]
    {
        let invite = crate::agent::remote::invite::SignedInviteGrant::decode(&request.invite)
            .map_err(|e| {
                Error::invalid_params()
                    .data(serde_json::json!({"error": format!("invalid mesh invite: {}", e)}))
            })?;

        let mesh = agent.mesh().ok_or_else(|| {
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
            agent.join_mesh_invite(invite.clone()).await.map_err(|e| {
                Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
            })?;
        }

        Ok(MeshJoinInfo {
            joined: true,
            peer_id: runtime.peer_id().to_string(),
            mesh_id,
            mesh_name: invite.grant.mesh_name,
            inviter_peer_id: invite.grant.inviter_peer_id.to_string(),
            already_joined,
        })
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        let _ = request;
        Err(Error::method_not_found())
    }
}

pub async fn list_nodes(agent: &LocalAgentHandle) -> MeshNodesInfo {
    MeshNodesInfo {
        nodes: crate::control::remote::list_remote_nodes(agent).await,
    }
}

pub async fn create_invite(
    agent: &LocalAgentHandle,
    request: CreateMeshInviteRequest,
) -> Result<MeshInviteCreatedInfo, Error> {
    #[cfg(feature = "remote")]
    {
        let Some(mesh) = agent.mesh() else {
            return Err(Error::invalid_request()
                .data(serde_json::json!({"error": "mesh not bootstrapped - start with --mesh"})));
        };

        if !mesh.is_iroh_transport_internal() {
            return Err(Error::invalid_request().data(serde_json::json!({
                "error": "mesh invites require iroh transport; restart host with --mesh --mesh-invite (or set transport=iroh)"
            })));
        }

        let ttl_secs = request
            .ttl
            .as_deref()
            .and_then(crate::agent::remote::invite::parse_duration_secs)
            .or(Some(24 * 3600));

        let invite = mesh
            .create_invite(request.mesh_name.clone(), ttl_secs, request.max_uses, false)
            .map_err(|e| {
                Error::internal_error().data(serde_json::json!({"error": format!("{e}")}))
            })?;
        let scope = crate::agent::remote::scope::MeshScopeId::Iroh {
            mesh_id: crate::agent::remote::invite::mesh_id_for(
                &invite.grant.inviter_peer_id,
                invite.grant.mesh_name.as_deref(),
            ),
        };
        agent
            .publish_mesh_scope(
                &crate::agent::remote::MeshRuntimeHandle::from(mesh.clone()),
                &scope,
            )
            .await
            .map_err(|e| {
                Error::internal_error().data(serde_json::json!({"error": format!("{e}")}))
            })?;

        let url = invite.to_url();
        let qr_code = crate::agent::remote::qr::render_to_terminal(&url);

        Ok(MeshInviteCreatedInfo {
            invite_id: invite.grant.invite_id,
            url,
            qr_code,
            expires_at: invite.grant.expires_at,
            max_uses: invite.grant.max_uses,
            mesh_name: request.mesh_name,
        })
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        let _ = request;
        Err(Error::method_not_found())
    }
}

pub async fn list_invites(agent: &LocalAgentHandle) -> Result<MeshInviteListInfo, Error> {
    #[cfg(feature = "remote")]
    {
        let Some(mesh) = agent.mesh() else {
            return Ok(MeshInviteListInfo {
                invites: Vec::new(),
            });
        };
        let invites = if let Some(store) = mesh.invite_store() {
            store
                .read()
                .list_pending()
                .into_iter()
                .map(map_invite_record)
                .collect()
        } else {
            Vec::new()
        };
        Ok(MeshInviteListInfo { invites })
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        Err(Error::method_not_found())
    }
}

pub async fn revoke_invite(
    agent: &LocalAgentHandle,
    request: RevokeMeshInviteRequest,
) -> Result<MeshInviteRevokedInfo, Error> {
    #[cfg(feature = "remote")]
    {
        let Some(mesh) = agent.mesh() else {
            return Ok(MeshInviteRevokedInfo {
                success: false,
                invite_id: request.invite_id,
                message: Some("mesh not bootstrapped - start with --mesh".to_string()),
            });
        };

        let result = if let Some(store) = mesh.invite_store() {
            store.write().revoke(&request.invite_id)
        } else {
            Err(crate::agent::remote::invite::InviteError::StoreError(
                "invite store not available".to_string(),
            ))
        };

        match result {
            Ok(()) => Ok(MeshInviteRevokedInfo {
                success: true,
                invite_id: request.invite_id,
                message: None,
            }),
            Err(e) => Ok(MeshInviteRevokedInfo {
                success: false,
                invite_id: request.invite_id,
                message: Some(e.to_string()),
            }),
        }
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        let _ = request;
        Err(Error::method_not_found())
    }
}
