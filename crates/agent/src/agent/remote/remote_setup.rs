//! Config-driven mesh bootstrap and remote agent registration.
//!
//! This module publishes local mesh actors and registers configured remote
//! agents against an already-started mesh runtime.
//!
//! All functionality is feature-gated behind `#[cfg(feature = "remote")]`.

use crate::agent::remote::mesh::MeshHandle;
use crate::agent::remote::provider_host::ProviderHostActor;
use crate::agent::remote::runtime_handle::MeshRuntimeHandle;
use crate::agent::remote::scope::{
    scoped_node_manager, scoped_node_manager_for_peer, scoped_provider_host,
};
use crate::config::RemoteAgentConfig;
use crate::delegation::{AgentInfo, DefaultAgentRegistry};
use anyhow::Result;
use std::sync::Arc;

/// Keepalive refs for local mesh actors registered after an agent is built.
#[cfg(feature = "remote")]
#[derive(Clone)]
pub struct LocalMeshActorRefs {
    pub node_manager: kameo::actor::ActorRef<crate::agent::remote::RemoteNodeManager>,
    pub provider_host: kameo::actor::ActorRef<ProviderHostActor>,
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

    let ph_dht_name = scoped_provider_host(scope, mesh.peer_id());
    runtime
        .register_actor(actor_refs.provider_host.clone(), ph_dht_name.clone())
        .await;
    log::info!("ProviderHostActor registered in DHT as '{}'", ph_dht_name);
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
    use crate::agent::remote::RemoteNodeManager;
    use kameo::actor::Spawn;

    let node_manager = RemoteNodeManager::new(
        handle.config.clone(),
        handle.registry.clone(),
        Some(mesh.clone()),
    );
    let node_manager = match node_name {
        Some(name) => node_manager.with_node_name(name),
        None => node_manager,
    };
    let node_manager_ref = RemoteNodeManager::spawn(node_manager);

    let provider_host = ProviderHostActor::new(handle.config.clone());
    let provider_host_ref = ProviderHostActor::spawn(provider_host);
    let actor_refs = LocalMeshActorRefs {
        node_manager: node_manager_ref,
        provider_host: provider_host_ref,
    };

    let runtime = MeshRuntimeHandle::from(mesh.clone());
    for scope in runtime.active_scopes() {
        register_local_mesh_actor_scope(mesh, &actor_refs, &scope).await;
    }

    actor_refs
}

pub async fn register_remote_agents_from_config(
    mesh: &MeshHandle,
    remotes: &[RemoteAgentConfig],
    peers: &[crate::config::MeshPeerConfig],
) -> Result<Arc<dyn crate::delegation::AgentRegistry + Send + Sync>> {
    let peer_map: std::collections::HashMap<&str, &str> = peers
        .iter()
        .map(|p| (p.name.as_str(), p.addr.as_str()))
        .collect();

    let mut registry = DefaultAgentRegistry::new();

    for remote in remotes {
        let peer_addr = match peer_map.get(remote.peer.as_str()) {
            Some(a) => *a,
            None => {
                log::warn!(
                    "remote_agent '{}' references unknown peer '{}'; skipping",
                    remote.id,
                    remote.peer
                );
                continue;
            }
        };

        match register_remote_agent(mesh, remote, peer_addr).await {
            Ok((agent_info, target_node_id)) => {
                log::info!(
                    "Registered remote agent '{}' (peer='{}')",
                    agent_info.id,
                    remote.peer
                );
                let remote_handle = Arc::new(super::remote_handle::RemoteAgentHandle::new(
                    remote.peer.clone(),
                    target_node_id,
                    mesh.clone(),
                ));
                registry.register_handle(agent_info, remote_handle);
            }
            Err(e) => {
                log::warn!(
                    "Could not register remote agent '{}' (peer='{}'): {}; skipping",
                    remote.id,
                    remote.peer,
                    e
                );
            }
        }
    }

    Ok(Arc::new(registry))
}

/// Attempt to contact the peer's `RemoteNodeManager` and confirm the agent
/// exists, then return an `AgentInfo` for registration.
#[tracing::instrument(
    name = "remote.setup.register_remote_agent",
    skip(mesh, _peer_addr),
    fields(
        peer = %remote.peer,
        agent_id = %remote.id,
        reachable = tracing::field::Empty,
        peer_hostname = tracing::field::Empty,
    )
)]
async fn register_remote_agent(
    mesh: &MeshHandle,
    remote: &RemoteAgentConfig,
    _peer_addr: &str,
) -> Result<(AgentInfo, Option<String>)> {
    use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};

    // Prefer peer-specific DHT names so multi-peer meshes do not bind to the
    // first generic node_manager provider returned by the DHT.
    let lookup_timeout = std::time::Duration::from_secs(5);
    let runtime = super::runtime_handle::MeshRuntimeHandle::from(mesh.clone());
    let mut found_ref = None;
    let mut target_node_id = None;

    if let Some(resolved) = mesh.resolve_peer_node_id(&remote.peer).await {
        let resolved_id = resolved.to_string();
        target_node_id = Some(resolved_id.clone());
        for scope in runtime.active_scopes() {
            let dht_name = super::scope::scoped_node_manager_for_peer(&scope, &resolved_id);
            let lookup_result = tokio::time::timeout(
                lookup_timeout,
                runtime.lookup_actor::<RemoteNodeManager>(dht_name.clone()),
            )
            .await;

            match lookup_result {
                Ok(Ok(Some(node_manager_ref))) => {
                    found_ref = Some(node_manager_ref);
                    break;
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    log::debug!(
                        "DHT lookup error for peer '{}' under '{}': {}",
                        remote.peer,
                        dht_name,
                        e
                    );
                }
                Err(_timeout) => {}
            }
        }
    }

    match found_ref {
        Some(node_manager_ref) => match node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo).await {
            Ok(node_info) => {
                tracing::Span::current()
                    .record("reachable", true)
                    .record("peer_hostname", &node_info.hostname);
                target_node_id = Some(node_info.node_id.to_string());
                log::debug!(
                    "Peer '{}' reachable (hostname='{}')",
                    remote.peer,
                    node_info.hostname
                );
            }
            Err(e) => {
                tracing::Span::current().record("reachable", false);
                log::debug!(
                    "GetNodeInfo failed for peer '{}': {} (registering anyway)",
                    remote.peer,
                    e
                );
            }
        },
        None => {
            tracing::Span::current().record("reachable", false);
            log::debug!(
                "Peer '{}' not yet in peer-specific scoped DHT; registering remote agent '{}' speculatively",
                remote.peer,
                remote.id
            );
        }
    }

    let info = AgentInfo {
        id: remote.id.clone(),
        name: remote.name.clone(),
        description: remote.description.clone(),
        capabilities: remote.capabilities.clone(),
        required_capabilities: vec![],
        meta: Some(serde_json::json!({
            "remote": true,
            "peer": remote.peer,
            "node_id": target_node_id,
        })),
    };

    Ok((info, target_node_id))
}
