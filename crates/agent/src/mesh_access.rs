//! SQLite policy composition for agent mesh entry points.

use std::{path::PathBuf, sync::Arc};

use querymt_agent_runtime::agent::remote::mesh;
use querymt_agent_sqlite::mesh_access::SqliteMeshAccess;
use querymt_remote::{
    IrohDnsConfig, MeshError, MeshRuntimeConfig, MeshRuntimeHandle, NodeId, SignedInviteGrant,
    peer_access::quota::QuotaStore,
};

pub(crate) async fn open_store(
    identity_file: Option<PathBuf>,
) -> Result<Arc<dyn QuotaStore>, MeshError> {
    let bootstrap_error = |error: &dyn std::fmt::Display| {
        MeshError::SwarmError(format!("mesh access startup: {error}"))
    };
    let identity_path = match identity_file {
        Some(path) => path,
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
    let store = SqliteMeshAccess::open_exclusive(PathBuf::from(store_path))
        .await
        .map_err(|e| bootstrap_error(&e))?;
    let identity = querymt_remote::identity::load_or_generate_keypair(Some(&identity_path))
        .map_err(|e| bootstrap_error(&e))?;
    let host = NodeId::from_peer_id(identity.public().to_peer_id());
    store
        .reconcile_startup(&host)
        .await
        .map_err(|e| bootstrap_error(&e))?;
    Ok(Arc::new(store))
}

/// Start an agent host with policy installed before accepting mesh RPCs.
pub async fn bootstrap_mesh_runtime(
    config: &MeshRuntimeConfig,
) -> Result<MeshRuntimeHandle, MeshError> {
    let store = open_store(config.identity_file.clone()).await?;
    mesh::bootstrap_mesh_runtime(config, store).await
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
    mesh::join_mesh_via_invite(invite, identity_file, iroh_gso, iroh_dns, open_store).await
}

#[cfg(test)]
mod tests {
    use crate::agent::remote::mesh::join_mesh_via_invite_with_dns;
    use querymt_remote::invite::InviteStore;

    use super::{NodeId, SqliteMeshAccess, open_store};
    use querymt_remote::identity::load_or_generate_keypair;
    use querymt_remote::peer_access::quota::{
        QuotaAttempt, QuotaEdit, QuotaLimit, QuotaScope, QuotaWindow,
    };
    use querymt_remote::peer_access::{
        ModelSelection, PeerGrant, PeerGrantKey, ProviderGrant, SharingProtocol,
    };
    use uuid::Uuid;

    #[tokio::test]
    async fn opening_mesh_store_reconciles_attempts_for_the_persisted_identity() {
        let dir = tempfile::tempdir().unwrap();
        let identity = dir.path().join("mesh").join("node.key");
        let store = open_store(Some(identity.clone())).await.unwrap();
        let host = load_or_generate_keypair(Some(&identity))
            .unwrap()
            .public()
            .to_peer_id();
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(host),
            peer: NodeId::from_peer_id(libp2p::PeerId::random()),
        };
        let policy = PeerGrant::Custom {
            sessions: false,
            filesystem: false,
            providers: vec![ProviderGrant {
                provider_id: "fixture".into(),
                models: ModelSelection::All,
            }],
        };
        let grant = store
            .replace_grant(&key, Uuid::nil(), policy.clone())
            .await
            .unwrap();
        let scope = QuotaScope {
            provider_id: "fixture".into(),
            model_id: None,
        };
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let revision = store
            .edit_quota(
                &key,
                grant.revision,
                &scope,
                QuotaEdit::Reconfigure(QuotaLimit::Finite {
                    tokens: 100,
                    window: QuotaWindow::Manual,
                }),
                now,
            )
            .await
            .unwrap();
        let attempt = store
            .reserve(QuotaAttempt {
                peer: key.clone(),
                grant_revision: revision,
                protocol: SharingProtocol::V1,
                provider_id: "fixture".into(),
                model_id: "model".into(),
                request_id: Uuid::now_v7(),
                bound_tokens: Some(40),
                now,
            })
            .await
            .unwrap();
        store.mark_dispatched(attempt).await.unwrap();
        drop(store);

        let reopened = open_store(Some(identity)).await.unwrap();
        let (grant, quotas) = reopened.access_snapshot(&key, now).await.unwrap();
        assert_eq!(grant.revision, revision);
        assert_eq!(grant.grant, policy);
        let provider_quota = quotas.iter().find(|quota| quota.scope == scope).unwrap();
        assert_eq!(provider_quota.reserved_tokens, 0);
        assert_eq!(provider_quota.estimated_tokens, 40);
    }

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
        let store_path = dir.path().join("join.key.access.sqlite3");
        let mut invalid = invite.clone();
        invalid.signature.clear();
        let error = join_mesh_via_invite_with_dns(&invalid, Some(identity.clone()), false, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("signature"));
        assert!(!store_path.exists());

        let _lease = SqliteMeshAccess::open_exclusive(store_path).await.unwrap();
        let error = join_mesh_via_invite_with_dns(&invite, Some(identity.clone()), false, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("mesh access startup"));
        assert!(!identity.exists());
    }
}
