//! Invite admission flow for attaching a running mesh handle to a remote mesh.

use anyhow::{Result, anyhow};

use crate::agent::remote::MeshScopeId;

pub(crate) async fn admit_via_invite_on_runtime(
    mesh: &mut crate::agent::remote::MeshHandle,
    invite: &crate::agent::remote::invite::SignedInviteGrant,
) -> Result<()> {
    use crate::agent::remote::node_manager::{AdmissionRequest, AdmissionResponse};

    invite.verify().map_err(|e| anyhow!(e.to_string()))?;
    let mesh_id = crate::agent::remote::invite::mesh_id_for(
        &invite.grant.inviter_peer_id,
        invite.grant.mesh_name.as_deref(),
    );

    let mesh_state_path = crate::agent::remote::mesh_state::default_mesh_state_path()?;
    let mut mesh_state =
        crate::agent::remote::mesh_state::MeshStateStore::load_or_create(&mesh_state_path)
            .map_err(|e| anyhow!(e.to_string()))?;

    let (existing_token, fallback_peers) = match mesh_state.get(&mesh_id) {
        Some(entry)
            if entry.status == crate::agent::remote::mesh_state::MeshStatus::Active
                && entry
                    .membership_token
                    .as_ref()
                    .is_some_and(|token| !token.is_expired()) =>
        {
            (
                entry.membership_token.clone(),
                entry.known_peers.values().cloned().collect(),
            )
        }
        _ => (None, vec![]),
    };

    let request = match existing_token {
        Some(token) => AdmissionRequest::Token {
            membership_token: token,
            peer_id: mesh.peer_id().to_string(),
        },
        None => AdmissionRequest::Invite {
            invite_id: invite.grant.invite_id.clone(),
            mesh_name: invite.grant.mesh_name.clone(),
            peer_id: mesh.peer_id().to_string(),
        },
    };

    let admission_scope = MeshScopeId::Iroh {
        mesh_id: mesh_id.clone(),
    };

    // Join the target scope early so admission requests can reuse any peers we
    // already know about for the mesh, even before membership is confirmed.
    mesh.join_iroh_scope(
        &mesh_id,
        crate::agent::remote::admission::admission_candidates(
            &invite.grant.inviter_peer_id,
            &fallback_peers,
        )
        .map(|candidates| {
            candidates
                .into_iter()
                .map(|candidate| candidate.peer_id)
                .collect()
        })
        .unwrap_or_default(),
    );

    let response = crate::agent::remote::admission::AdmissionService::new(
        crate::agent::remote::admission::MeshAdmissionTransport::new(mesh.clone()),
        crate::agent::remote::admission::AdmissionPolicy::production(),
    )
    .admit(
        &mesh_id,
        admission_scope,
        &invite.grant.inviter_peer_id,
        &fallback_peers,
        request.clone(),
    )
    .await?;

    match response {
        AdmissionResponse::Admitted {
            membership_token,
            existing_peers,
        } => {
            let known_peers = known_peers_from_strings(mesh, &mesh_id, &existing_peers);
            let admitted_peer_ids = peer_ids_from_strings(&existing_peers);
            mesh_state
                .upsert_joined_mesh(membership_token, known_peers)
                .map_err(|e| anyhow!("failed to persist mesh state: {e}"))?;
            mesh.join_iroh_scope(&mesh_id, admitted_peer_ids);
        }
        AdmissionResponse::Readmitted { existing_peers } => {
            let known_peers = known_peers_from_strings(mesh, &mesh_id, &existing_peers);
            let admitted_peer_ids = peer_ids_from_strings(&existing_peers);
            mesh_state
                .update_known_peers(&mesh_id, known_peers)
                .map_err(|e| anyhow!("failed to update mesh state: {e}"))?;
            mesh.join_iroh_scope(&mesh_id, admitted_peer_ids);
        }
        AdmissionResponse::Rejected { reason } => {
            return Err(anyhow!("admission rejected: {reason}"));
        }
    }

    if let Some(store_arc) = mesh.mesh_state_store() {
        let fresh =
            crate::agent::remote::mesh_state::MeshStateStore::load_or_create(&mesh_state_path)
                .map_err(|e| anyhow!(e.to_string()))?;
        *store_arc.write() = fresh;
    }
    mesh.ensure_scope(MeshScopeId::Iroh {
        mesh_id: mesh_id.clone(),
    });
    let _ = mesh.subscribe_peer_events().resubscribe().try_recv();
    Ok(())
}

fn peer_ids_from_strings(existing_peers: &[String]) -> Vec<libp2p::PeerId> {
    existing_peers
        .iter()
        .filter_map(|peer_str| peer_str.parse().ok())
        .collect()
}

fn known_peers_from_strings(
    mesh: &crate::agent::remote::MeshHandle,
    mesh_id: &str,
    existing_peers: &[String],
) -> Vec<crate::agent::remote::invite::PeerEntry> {
    let mut all_peer_strs: Vec<String> = mesh
        .known_peer_ids()
        .into_iter()
        .map(|pid| pid.to_string())
        .collect();
    for peer_str in existing_peers {
        if let Ok(pid) = peer_str.parse() {
            mesh.dial_existing_iroh_peer(
                &pid,
                MeshScopeId::Iroh {
                    mesh_id: mesh_id.to_string(),
                },
            );
        }
        if !all_peer_strs.contains(peer_str) {
            all_peer_strs.push(peer_str.clone());
        }
    }
    all_peer_strs
        .into_iter()
        .map(|pid| crate::agent::remote::invite::PeerEntry {
            peer_id: pid.clone(),
            addrs: vec![format!("/p2p/{pid}")],
        })
        .collect()
}
