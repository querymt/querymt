//! Config-driven mesh bootstrap and remote agent registration.
//!
//! This module publishes local mesh actors and registers configured remote
//! agents against an already-started mesh runtime.
//!
//! All functionality is feature-gated behind `#[cfg(feature = "remote")]`.

use crate::agent::remote::scope::{
    scoped_node_manager, scoped_node_manager_for_peer, scoped_provider_host,
};
use crate::config::RemoteAgentConfig;
use crate::delegation::AgentInfo;
use crate::delegation::roster::{AgentId, Member, MemberAgent, MemberOrigin, RemoteMember};
use anyhow::Result;
use querymt_remote::scoped_provider_catalog;
use querymt_remote::{MeshRuntimeHandle, ProviderCatalogActor, ProviderHostActor};
use std::sync::Arc;

/// Keepalive refs for local mesh actors registered after an agent is built.
#[cfg(feature = "remote")]
#[derive(Clone)]
pub struct LocalMeshActorRefs {
    pub node_manager: kameo::actor::ActorRef<crate::agent::remote::RemoteNodeManager>,
    pub provider_host: kameo::actor::ActorRef<ProviderHostActor>,
    pub provider_catalog: kameo::actor::ActorRef<ProviderCatalogActor>,
}

#[cfg(feature = "remote")]
pub async fn register_local_mesh_actor_scope(
    mesh: &crate::agent::remote::MeshHandle,
    actor_refs: &LocalMeshActorRefs,
    scope: &crate::agent::remote::scope::MeshScopeId,
) {
    let runtime = MeshRuntimeHandle::from(mesh.clone());

    let node_name = scoped_node_manager(scope);
    runtime
        .register_actor(actor_refs.node_manager.clone(), node_name.clone())
        .await;
    log::info!("RemoteNodeManager registered in DHT as '{}'", node_name);

    let per_peer_name = scoped_node_manager_for_peer(scope, mesh.peer_id());
    runtime
        .register_actor(actor_refs.node_manager.clone(), per_peer_name.clone())
        .await;
    log::info!(
        "RemoteNodeManager also registered in DHT as '{}'",
        per_peer_name
    );

    // Guarded meshes do not publish or register provider routes until metering
    // is available. Legacy meshes keep their original actor registrations.
    if mesh.inbound_router().is_some() {
        return;
    }

    let ph_dht_name = scoped_provider_host(scope, mesh.peer_id());
    runtime
        .register_actor(actor_refs.provider_host.clone(), ph_dht_name.clone())
        .await;
    log::info!("ProviderHostActor registered in DHT as '{}'", ph_dht_name);

    let provider_catalog_name = scoped_provider_catalog(scope, mesh.peer_id());
    runtime
        .register_actor(
            actor_refs.provider_catalog.clone(),
            provider_catalog_name.clone(),
        )
        .await;
    log::info!(
        "ProviderCatalogActor registered in DHT as '{}'",
        provider_catalog_name
    );
}

/// Spawn and register the local mesh-facing actors for an already-built agent.
///
/// This is the post-build step needed when the caller already has a live
/// `LocalAgentHandle` with mesh attached and wants to advertise local session and
/// provider services to peers.
#[cfg(feature = "remote")]
pub async fn spawn_and_register_local_mesh_actors(
    handle: &crate::agent::LocalAgentHandle,
    mesh: &crate::agent::remote::MeshHandle,
) -> LocalMeshActorRefs {
    spawn_and_register_local_mesh_actors_with_name(handle, mesh, None).await
}

/// Like [`spawn_and_register_local_mesh_actors`] but overrides the node name
/// advertised to mesh peers (useful on mobile where OS hostname is "unknown").
#[cfg(feature = "remote")]
pub async fn spawn_and_register_local_mesh_actors_with_name(
    handle: &crate::agent::LocalAgentHandle,
    mesh: &crate::agent::remote::MeshHandle,
    node_name: Option<String>,
) -> LocalMeshActorRefs {
    let _ = handle.model_inventory.ensure_local_refresh().await;
    use crate::agent::remote::RemoteNodeManager;
    use kameo::actor::Spawn;

    let node_manager = RemoteNodeManager::new(
        handle.config.clone(),
        handle.registry.clone(),
        Some(mesh.clone()),
        handle.scheduler_handle.clone(),
    )
    .with_profiles_slot(handle.profiles.clone());
    let node_manager = match node_name.clone() {
        Some(name) => node_manager.with_node_name(name),
        None => node_manager,
    };
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);
    if let (Some(router), Some(store)) = (mesh.inbound_router(), mesh.quota_store()) {
        RemoteNodeManager::register_guarded_discovery(router, store.clone(), &node_manager_ref);
        crate::agent::handle::ext_mesh::register_peer_access_read(
            router,
            store.clone(),
            node_manager_ref.id(),
        );
    }

    let provider_host = crate::agent::remote::provider_host_backend::provider_host_from_config(
        handle.config.clone(),
    );
    let provider_host_ref = ProviderHostActor::spawn(provider_host);
    let provider_catalog = ProviderCatalogActor::new(Arc::new(
        crate::agent::remote::provider_catalog_backend::AgentProviderCatalogBackend::new(
            handle.config.clone(),
            handle.model_inventory.clone(),
            crate::agent::remote::NodeId::from_peer_id(*mesh.peer_id()).to_string(),
            node_name.clone(),
        ),
    ));
    let provider_catalog_ref = ProviderCatalogActor::spawn(provider_catalog);
    let actor_refs = LocalMeshActorRefs {
        node_manager: node_manager_ref,
        provider_host: provider_host_ref,
        provider_catalog: provider_catalog_ref,
    };

    let runtime = MeshRuntimeHandle::from(mesh.clone());
    for scope in runtime.active_scopes() {
        register_local_mesh_actor_scope(mesh, &actor_refs, &scope).await;
    }

    actor_refs
}

/// Builds the members for `[[remote_agents]]` without contacting any peer: a
/// member is listed whether or not its peer is reachable, and the node is
/// resolved when a delegation needs it. The owner id in a configured peer
/// address (`/p2p/<id>`) is kept as a fallback for that resolution.
pub fn remote_members_from_config(
    remotes: &[RemoteAgentConfig],
    peers: &[crate::config::MeshPeerConfig],
) -> Result<Vec<Member>> {
    use libp2p::multiaddr::Protocol;

    let peer_map: std::collections::HashMap<&str, &str> = peers
        .iter()
        .map(|p| (p.name.as_str(), p.addr.as_str()))
        .collect();

    let mut members = Vec::new();
    for remote in remotes {
        let Some(peer_addr) = peer_map.get(remote.peer.as_str()) else {
            log::warn!(
                "remote_agent '{}' references unknown peer '{}'; skipping",
                remote.id,
                remote.peer
            );
            continue;
        };
        // A terminal /p2p address component identifies the owner without a
        // name resolver.
        let node_id = peer_addr
            .parse::<libp2p::Multiaddr>()
            .ok()
            .and_then(|addr| match addr.iter().last()? {
                Protocol::P2p(peer_id) => {
                    Some(crate::agent::remote::NodeId::from_peer_id(peer_id).to_string())
                }
                _ => None,
            });
        let info = AgentInfo {
            isolation: Default::default(),
            id: AgentId::try_from(remote.id.as_str())?,
            name: remote.name.clone(),
            description: remote.description.clone(),
            capabilities: remote.capabilities.clone(),
            required_capabilities: vec![],
            meta: None,
        };
        members.push(Member::built(
            info,
            MemberOrigin::Config,
            MemberAgent::Remote(RemoteMember::new(remote.peer.clone(), node_id)),
        ));
    }
    Ok(members)
}
