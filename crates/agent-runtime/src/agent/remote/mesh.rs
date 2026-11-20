//! Agent-owned mesh entry points that remain after moving the concrete runtime to
//! `querymt-remote`.
//!
//! The reusable runtime/bootstrap lives in `querymt-remote`. Agent keeps only the
//! invite-join adapter because it still talks to agent admission/node-manager code.

mod join;

pub use querymt_remote::{
    MeshError, MeshEvent, MeshHandle, MeshRoute, MeshRuntimeConfig, MeshRuntimeHandle, MeshScopeId,
    PeerEvent, RouteTable,
};

use querymt_remote::SignedInviteGrant;

/// Start an agent host with policy installed before accepting mesh RPCs.
pub async fn bootstrap_mesh_runtime(
    config: &MeshRuntimeConfig,
) -> Result<MeshRuntimeHandle, MeshError> {
    use querymt_remote::{NodeId, peer_access::quota::QuotaStore};
    let bootstrap_error = |error: &dyn std::fmt::Display| {
        MeshError::SwarmError(format!("mesh access startup: {error}"))
    };
    let identity_path = match &config.identity_file {
        Some(path) => path.clone(),
        None => {
            querymt_remote::identity::default_identity_path().map_err(|e| bootstrap_error(&e))?
        }
    };
    if let Some(parent) = identity_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| bootstrap_error(&error))?;
    }
    let mut store_path = identity_path.as_os_str().to_os_string();
    store_path.push(".access.sqlite3");
    let store = std::sync::Arc::new(
        querymt_agent_sqlite::mesh_access::SqliteMeshAccess::open_exclusive(
            std::path::PathBuf::from(store_path),
        )
        .await
        .map_err(|e| bootstrap_error(&e))?,
    );
    let identity = querymt_remote::identity::load_or_generate_keypair(Some(&identity_path))
        .map_err(|e| bootstrap_error(&e))?;
    let host = NodeId::from_peer_id(identity.public().to_peer_id());
    store
        .reconcile_startup(&host)
        .await
        .map_err(|e| bootstrap_error(&e))?;
    querymt_remote::bootstrap_mesh_runtime_with_policy(config, store).await
}

pub async fn join_mesh_via_invite(
    invite: &SignedInviteGrant,
    identity_file: Option<std::path::PathBuf>,
) -> Result<MeshRuntimeHandle, MeshError> {
    join_mesh_via_invite_with_gso(invite, identity_file, true).await
}

/// Join or reconnect via an invite with an explicit iroh actor-endpoint GSO setting.
/// The default join entry point keeps GSO enabled.
pub async fn join_mesh_via_invite_with_gso(
    invite: &SignedInviteGrant,
    identity_file: Option<std::path::PathBuf>,
    iroh_gso: bool,
) -> Result<MeshRuntimeHandle, MeshError> {
    join_mesh_via_invite_with_dns(invite, identity_file, iroh_gso, None).await
}

/// Join with explicit endpoint GSO and DNS policy, including stored-membership reconnects.
pub async fn join_mesh_via_invite_with_dns(
    invite: &SignedInviteGrant,
    identity_file: Option<std::path::PathBuf>,
    iroh_gso: bool,
    iroh_dns: Option<querymt_remote::IrohDnsConfig>,
) -> Result<MeshRuntimeHandle, MeshError> {
    join::join_mesh_via_invite(invite, identity_file, iroh_gso, iroh_dns).await
}
