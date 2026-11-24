//! Guarded agent mesh entry points.

use std::path::PathBuf;

use querymt_agent_runtime::agent::remote::mesh;
use querymt_remote::{
    IrohDnsConfig, MeshError, MeshRuntimeConfig, MeshRuntimeHandle, SignedInviteGrant,
};

/// Start an agent host with policy installed before accepting mesh RPCs.
pub async fn bootstrap_mesh_runtime(
    config: &MeshRuntimeConfig,
) -> Result<MeshRuntimeHandle, MeshError> {
    querymt_remote::bootstrap_mesh_runtime_guarded(config).await
}

/// Join or reconnect via an invite using the default endpoint settings.
pub async fn join_mesh_via_invite(
    invite: &SignedInviteGrant,
    identity_file: Option<PathBuf>,
) -> Result<MeshRuntimeHandle, MeshError> {
    join_mesh_via_invite_with_gso(invite, identity_file, true).await
}

/// Join or reconnect via an invite with an explicit iroh actor-endpoint GSO setting.
/// The default join entry point keeps GSO enabled.
pub async fn join_mesh_via_invite_with_gso(
    invite: &SignedInviteGrant,
    identity_file: Option<PathBuf>,
    iroh_gso: bool,
) -> Result<MeshRuntimeHandle, MeshError> {
    join_mesh_via_invite_with_dns(invite, identity_file, iroh_gso, None).await
}

/// Join with explicit endpoint GSO and DNS policy, including stored-membership reconnects.
pub async fn join_mesh_via_invite_with_dns(
    invite: &SignedInviteGrant,
    identity_file: Option<PathBuf>,
    iroh_gso: bool,
    iroh_dns: Option<IrohDnsConfig>,
) -> Result<MeshRuntimeHandle, MeshError> {
    mesh::join_mesh_via_invite(invite, identity_file, iroh_gso, iroh_dns).await
}

#[cfg(test)]
mod tests {
    use super::{bootstrap_mesh_runtime, join_mesh_via_invite, join_mesh_via_invite_with_dns};
    use querymt_remote::invite::InviteStore;

    #[tokio::test]
    async fn join_checks_invite_before_opening_required_policy_store() {
        let dir = tempfile::tempdir().unwrap();
        let key = libp2p::identity::Keypair::generate_ed25519();
        let invite = InviteStore::load_or_create(&dir.path().join("invites.json"))
            .unwrap()
            .create_invite(
                &key,
                &key.public().to_peer_id().to_string(),
                None,
                None,
                1,
                Default::default(),
            )
            .unwrap();
        let identity = dir.path().join("join.key");
        let store_path = dir.path().join("join.key.mesh_state.json");
        let mut invalid = invite.clone();
        invalid.signature.clear();
        let error = join_mesh_via_invite_with_dns(&invalid, Some(identity.clone()), false, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("signature"));
        assert!(!store_path.exists());

        std::fs::write(&store_path, "not JSON").unwrap();
        let error = join_mesh_via_invite(&invite, Some(identity.clone()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mesh access startup"));
        assert!(!identity.exists());

        // Direct host startup, used by qmtcode, must fail closed on the same corrupt state.
        let error = bootstrap_mesh_runtime(&querymt_remote::MeshRuntimeConfig {
            enabled: true,
            lan: None,
            iroh_enabled: true,
            iroh_gso: true,
            iroh_dns: None,
            iroh_scopes: vec![],
            identity_file: Some(identity.clone()),
            request_timeout: std::time::Duration::from_secs(5),
            stream_reconnect_grace: std::time::Duration::from_secs(5),
            node_name: None,
            peers: vec![],
            auto_fallback: false,
            access: Default::default(),
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("mesh access startup"));
        assert!(!identity.exists());
        assert_eq!(std::fs::read_to_string(store_path).unwrap(), "not JSON");
    }
}
