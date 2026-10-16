use crate::agent::remote::mesh::{MeshError, MeshScopeId};
use libp2p::PeerId;
use querymt_remote::{IrohMeshConfig, MeshRuntimeConfig, MeshRuntimeHandle};

/// Join an existing mesh using a signed invite grant.
///
/// Steps:
/// 1. Verify the invite grant signature and check expiry.
/// 2. Bootstrap the iroh swarm with the selected identity and dial the inviter.
/// 3. Check that identity's mesh state for an existing token and fallback peers.
///    If found, use `AdmissionRequest::Token`; otherwise use
///    `AdmissionRequest::Invite` (first join, consumes one invite use).
/// 4. Send the admission request to the target peer's `RemoteNodeManager`.
/// 5. On `Admitted` - persist the returned `MembershipToken` and known peers.
/// 6. On `Rejected` - disconnect and return an error.
///
/// After this call the local node is a full mesh member and can discover other
/// members via Kademlia / iroh relay.
///
/// # Arguments
/// - `invite` - a decoded `SignedInviteGrant` (signature verified offline)
/// - `identity_file` - optional path to the persistent ed25519 identity file
/// - `iroh_gso` - whether to enable GSO on the actor endpoint
pub async fn join_mesh_via_invite(
    invite: &crate::agent::remote::invite::SignedInviteGrant,
    identity_file: Option<std::path::PathBuf>,
    iroh_gso: bool,
) -> Result<MeshRuntimeHandle, MeshError> {
    use crate::agent::remote::invite::{PeerEntry, mesh_id_for};
    use crate::agent::remote::node_manager::{AdmissionRequest, AdmissionResponse};

    invite
        .verify()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    let mesh_id = mesh_id_for(
        &invite.grant.inviter_peer_id,
        invite.grant.mesh_name.as_deref(),
    );

    let config = MeshRuntimeConfig {
        enabled: true,
        lan: None,
        iroh_enabled: true,
        iroh_gso,
        iroh_scopes: vec![IrohMeshConfig {
            mesh_id: mesh_id.clone(),
            invite: Some(invite.encode()),
            name: invite.grant.mesh_name.clone(),
        }],
        identity_file,
        request_timeout: std::time::Duration::from_secs(300),
        stream_reconnect_grace: std::time::Duration::from_secs(120),
        node_name: None,
        peers: Vec::new(),
        auto_fallback: false,
    };
    let runtime = querymt_remote::bootstrap_mesh_runtime(&config).await?;
    let mesh = runtime.as_mesh_handle();
    let mesh_state = mesh
        .mesh_state_store()
        .cloned()
        .ok_or_else(|| MeshError::SwarmError("mesh state store unavailable".into()))?;
    let (existing_token, fallback_peers) = match mesh_state.read().get(&mesh_id) {
        Some(entry)
            if entry.status == crate::agent::remote::mesh_state::MeshStatus::Active
                && entry
                    .membership_token
                    .as_ref()
                    .is_some_and(|token| !token.is_expired()) =>
        {
            log::info!(
                "Found existing mesh state for mesh '{}', attempting reconnect",
                mesh_id
            );
            (
                entry.membership_token.clone(),
                entry.known_peers.values().cloned().collect(),
            )
        }
        _ => (None, vec![]),
    };

    log::info!(
        "Joining mesh via invite (inviter={}, name={:?})",
        invite.grant.inviter_peer_id,
        invite.grant.mesh_name
    );

    mesh.ensure_scope(MeshScopeId::Iroh {
        mesh_id: mesh_id.clone(),
    });
    let my_peer_id = mesh.peer_id().to_string();

    let request = match existing_token {
        Some(token) => AdmissionRequest::Token {
            membership_token: token,
            peer_id: my_peer_id.clone(),
        },
        None => AdmissionRequest::Invite {
            invite_id: invite.grant.invite_id.clone(),
            mesh_name: invite.grant.mesh_name.clone(),
            peer_id: my_peer_id.clone(),
        },
    };

    let admission_scope = MeshScopeId::Iroh {
        mesh_id: mesh_id.clone(),
    };
    mesh.join_iroh_scope(
        &mesh_id,
        admission_candidates_for_scope(&invite.grant.inviter_peer_id, &fallback_peers),
    );
    let response = crate::agent::remote::admission::AdmissionService::new(
        crate::agent::remote::admission::MeshAdmissionTransport::new(mesh.clone()),
        crate::agent::remote::admission::AdmissionPolicy::production(),
    )
    .admit(
        &mesh_id,
        admission_scope.clone(),
        &invite.grant.inviter_peer_id,
        &fallback_peers,
        request.clone(),
    )
    .await
    .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    match response {
        AdmissionResponse::Admitted {
            membership_token,
            existing_peers,
        } => {
            log::info!(
                "Admitted to mesh '{}' (admitted_by={}, {} existing peers)",
                mesh_id,
                membership_token.admitted_by,
                existing_peers.len(),
            );

            for peer_str in &existing_peers {
                if let Ok(pid) = peer_str.parse::<PeerId>() {
                    log::info!("Dialing existing mesh peer: {}", pid);
                    mesh.dial_existing_iroh_peer(&pid, admission_scope.clone());
                } else {
                    log::warn!("Ignoring invalid PeerId in existing_peers: {}", peer_str);
                }
            }

            let mut all_peer_strs: Vec<String> = mesh
                .known_peer_ids()
                .into_iter()
                .map(|pid| pid.to_string())
                .collect();
            for p in &existing_peers {
                if !all_peer_strs.contains(p) {
                    all_peer_strs.push(p.clone());
                }
            }
            let known_peers: Vec<PeerEntry> = all_peer_strs
                .into_iter()
                .map(|pid| PeerEntry {
                    peer_id: pid.clone(),
                    addrs: vec![format!("/p2p/{pid}")],
                })
                .collect();

            crate::agent::remote::admission::persist_admission(
                mesh_state.clone(),
                mesh_id.clone(),
                Some(membership_token),
                known_peers,
            )
            .await
            .map_err(|e| MeshError::SwarmError(e.to_string()))?;

            mesh.emit_scope_joined(MeshScopeId::Iroh {
                mesh_id: mesh_id.clone(),
            });
        }
        AdmissionResponse::Readmitted { existing_peers } => {
            log::info!(
                "Readmitted to mesh '{}' (token accepted, {} existing peers)",
                mesh_id,
                existing_peers.len(),
            );

            for peer_str in &existing_peers {
                if let Ok(pid) = peer_str.parse::<PeerId>() {
                    log::info!("Dialing existing mesh peer (readmit): {}", pid);
                    mesh.dial_existing_iroh_peer(&pid, admission_scope.clone());
                } else {
                    log::warn!("Ignoring invalid PeerId in existing_peers: {}", peer_str);
                }
            }

            let known_peers = existing_peers
                .iter()
                .map(|peer| PeerEntry {
                    peer_id: peer.clone(),
                    addrs: vec![format!("/p2p/{peer}")],
                })
                .chain(fallback_peers.iter().cloned())
                .collect();
            crate::agent::remote::admission::persist_admission(
                mesh_state.clone(),
                mesh_id.clone(),
                None,
                known_peers,
            )
            .await
            .map_err(|e| MeshError::SwarmError(e.to_string()))?;

            mesh.emit_scope_joined(MeshScopeId::Iroh {
                mesh_id: mesh_id.clone(),
            });
        }
        AdmissionResponse::Rejected { reason } => {
            log::warn!("Mesh admission rejected: {}", reason);
            return Err(MeshError::SwarmError(format!(
                "admission rejected: {reason}"
            )));
        }
    }

    Ok(runtime)
}

/// Find a reachable `RemoteNodeManager` for the admission handshake.
///
/// Tries the original inviter first, then falls back to cached peers.
pub(super) fn admission_candidates_for_scope(
    inviter_peer_id: &str,
    fallback_peers: &[crate::agent::remote::invite::PeerEntry],
) -> Vec<PeerId> {
    crate::agent::remote::admission::admission_candidates(inviter_peer_id, fallback_peers)
        .map(|candidates| {
            candidates
                .into_iter()
                .map(|candidate| candidate.peer_id)
                .collect()
        })
        .unwrap_or_default()
}
