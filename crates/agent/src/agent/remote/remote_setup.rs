//! Config-driven mesh bootstrap and remote agent registration.
//!
//! This module implements reading `[mesh]` and `[[remote_agents]]`
//! from TOML config and automatically:
//! 1. Bootstrapping the kameo libp2p swarm.
//! 2. Registering the local node as a `RemoteNodeManager` in the DHT.
//! 3. For each declared `[[remote_agents]]`, looking up the peer's
//!    `RemoteNodeManager` via DHT and registering the remote agent in the
//!    local `DefaultAgentRegistry` so the planner can delegate to it.
//!
//! All functionality is feature-gated behind `#[cfg(feature = "remote")]`.

use crate::agent::remote::mesh::{MeshHandle, bootstrap_mesh_runtime};
use crate::agent::remote::mesh_runtime_config::MeshRuntimeConfig;
use crate::agent::remote::provider_host::ProviderHostActor;
use crate::agent::remote::runtime_handle::MeshRuntimeHandle;
use crate::agent::remote::scope::{
    scoped_node_manager, scoped_node_manager_for_peer, scoped_provider_host,
};
use crate::config::{MeshTomlConfig, RemoteAgentConfig};
use crate::delegation::{AgentInfo, DefaultAgentRegistry};
use anyhow::Result;
use std::sync::Arc;
use tracing::Instrument;

/// Result of a successful mesh setup from config.
pub struct MeshSetupResult {
    /// Handle to the bootstrapped swarm.
    pub mesh: MeshHandle,
    /// Registry pre-populated with remote agent `AgentInfo` entries.
    /// Wrap in `Arc` before passing to `AgentConfigBuilder::with_agent_registry`.
    pub registry: DefaultAgentRegistry,
    /// The spawned `ProviderHostActor` ref (registered in DHT as
    /// `"provider_host::peer::{peer_id}"`).  `None` when `agent_config` was not
    /// provided to `setup_mesh_from_config`.
    pub provider_host: Option<kameo::actor::ActorRef<ProviderHostActor>>,
}

/// Keepalive refs for local mesh actors registered after an agent is built.
#[cfg(feature = "remote")]
pub struct LocalMeshActorRefs {
    pub node_manager: kameo::actor::ActorRef<crate::agent::remote::RemoteNodeManager>,
    pub provider_host: kameo::actor::ActorRef<ProviderHostActor>,
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
    let runtime = MeshRuntimeHandle::from(mesh.clone());

    for scope in runtime.active_scopes() {
        let node_name = scoped_node_manager(&scope);
        runtime
            .register_actor(node_manager_ref.clone(), node_name.clone())
            .await;
        log::info!("RemoteNodeManager registered in DHT as '{}'", node_name);

        let per_peer_name = scoped_node_manager_for_peer(&scope, mesh.peer_id());
        runtime
            .register_actor(node_manager_ref.clone(), per_peer_name.clone())
            .await;
        log::info!(
            "RemoteNodeManager also registered in DHT as '{}'",
            per_peer_name
        );
    }

    let provider_host = ProviderHostActor::new(handle.config.clone());
    let provider_host_ref = ProviderHostActor::spawn(provider_host);
    for scope in runtime.active_scopes() {
        let ph_dht_name = scoped_provider_host(&scope, mesh.peer_id());
        runtime
            .register_actor(provider_host_ref.clone(), ph_dht_name.clone())
            .await;
        log::info!("ProviderHostActor registered in DHT as '{}'", ph_dht_name);
    }

    // ── Re-publish DHT records to already-connected peers ──────────────────
    //
    // The mesh swarm may have connected to peers during bootstrap (before this
    // function was called).  The `re_register_fns` stored by `register_actor`
    // only fire on *future* connections, so peers already connected at this
    // point never receive the DHT records.  Re-registering pushes the records
    // into the kameo DHT, making them visible to already-connected peers
    // through standard Kademlia replication.
    let connected_count = mesh.known_peer_ids().len();
    if connected_count > 0 {
        log::info!(
            "Re-publishing DHT records to {} already-connected peer(s)",
            connected_count
        );
        for scope in runtime.active_scopes() {
            let nm_name = scoped_node_manager(&scope);
            runtime
                .register_actor(node_manager_ref.clone(), nm_name.clone())
                .await;
            log::info!("Re-published RemoteNodeManager as '{}'", nm_name);

            let pp_name = scoped_node_manager_for_peer(&scope, mesh.peer_id());
            runtime
                .register_actor(node_manager_ref.clone(), pp_name.clone())
                .await;

            let ph_name = scoped_provider_host(&scope, mesh.peer_id());
            runtime
                .register_actor(provider_host_ref.clone(), ph_name.clone())
                .await;
        }
    }

    LocalMeshActorRefs {
        node_manager: node_manager_ref,
        provider_host: provider_host_ref,
    }
}

/// Bootstrap the kameo mesh and register remote agents from TOML config.
///
/// Call this **before** building the `AgentHandle` so that the registry
/// returned here can be passed to `AgentConfigBuilder::with_agent_registry`.
///
/// # Arguments
/// * `mesh_cfg`   — the `[mesh]` section from TOML.
/// * `remotes`    — the `[[remote_agents]]` entries from TOML.
/// * `node_manager_ref` — optionally an already-spawned `RemoteNodeManager`
///   actor ref that should be registered in the DHT.  Pass `None` if the
///   node does not want to accept incoming session creation requests.
/// * `agent_config` — when `Some`, a `ProviderHostActor` is spawned and
///   registered in the DHT as `"provider_host::peer::{peer_id}"`, making this
///   node's providers available to the mesh.  Pass `None` to skip.
///
/// # Returns
/// A [`MeshSetupResult`] containing the live [`MeshHandle`] and a
/// [`DefaultAgentRegistry`] with one entry per reachable remote agent.
/// Remote agents that are not reachable at startup are logged and skipped
/// — they can be re-registered at runtime once the peer becomes available.
#[tracing::instrument(
    name = "remote.setup.setup_mesh_from_config",
    skip(mesh_cfg, node_manager_ref, agent_config),
    fields(
        discovery = ?mesh_cfg.discovery,
        listen = mesh_cfg.listen.as_deref().unwrap_or("<auto>"),
        peer_count = mesh_cfg.peers.len(),
        remote_agent_count = remotes.len(),
        peer_id = tracing::field::Empty,
    )
)]
pub async fn setup_mesh_from_config(
    mesh_cfg: &MeshTomlConfig,
    remotes: &[RemoteAgentConfig],
    node_manager_ref: Option<kameo::actor::ActorRef<crate::agent::remote::RemoteNodeManager>>,
    agent_config: Option<Arc<crate::agent::agent_config::AgentConfig>>,
) -> Result<MeshSetupResult> {
    // ── 1. Normalize TOML mesh config into MeshRuntimeConfig ─────────────────

    let runtime_config = MeshRuntimeConfig::from_toml_config(
        mesh_cfg.enabled,
        mesh_cfg.transport.clone(),
        mesh_cfg.discovery.clone(),
        mesh_cfg.listen.clone(),
        mesh_cfg.peers.iter().map(|p| p.addr.clone()).collect(),
        mesh_cfg.request_timeout_secs,
        mesh_cfg.stream_reconnect_grace_secs,
        mesh_cfg.identity_file.clone(),
        mesh_cfg.invite.clone(),
        mesh_cfg.node_name.clone(),
        mesh_cfg.auto_fallback,
        mesh_cfg.lan.clone(),
        mesh_cfg.iroh.clone(),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let listen_addr_str = runtime_config
        .lan
        .as_ref()
        .and_then(|l| l.listen.as_deref())
        .unwrap_or("<auto>")
        .to_string();

    // ── 2. Bootstrap the libp2p swarm ─────────────────────────────────────────

    let runtime = {
        let bootstrap_span = tracing::info_span!(
            "remote.setup.bootstrap_mesh_runtime",
            listen = %listen_addr_str,
            transports = ?runtime_config.enabled_transports(),
            scopes = ?runtime_config.active_scopes(),
        );
        bootstrap_mesh_runtime(&runtime_config)
            .instrument(bootstrap_span)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    };
    let mesh = runtime.as_mesh_handle().clone();
    tracing::Span::current().record("peer_id", mesh.peer_id().to_string());
    log::info!("Kameo mesh bootstrapped (peer_id={})", mesh.peer_id());

    // ── 3. Register the local RemoteNodeManager in the DHT (if provided) ──────

    if let Some(nm_ref) = node_manager_ref {
        for scope in runtime.active_scopes() {
            let dht_name = scoped_node_manager(&scope);
            let reg_span = tracing::info_span!(
                "remote.setup.register_node_manager",
                dht_name = %dht_name,
                scope = %scope,
            );
            runtime
                .register_actor(nm_ref.clone(), dht_name.clone())
                .instrument(reg_span)
                .await;
            log::info!(
                "Local RemoteNodeManager registered in DHT as '{}'",
                dht_name
            );

            // Also register under the per-peer name so find_node_manager can do a
            // direct O(1) lookup by peer_id.
            let per_peer_name = scoped_node_manager_for_peer(&scope, mesh.peer_id());
            runtime
                .register_actor(nm_ref.clone(), per_peer_name.clone())
                .await;
            log::info!(
                "Local RemoteNodeManager also registered in DHT as '{}'",
                per_peer_name
            );
        }
    }

    // ── 3b. Spawn and register ProviderHostActor (if agent_config provided) ──

    let provider_host = if let Some(config) = agent_config {
        use kameo::actor::Spawn;

        let hostname = get_hostname();

        let actor = ProviderHostActor::new(config);
        let actor_ref = ProviderHostActor::spawn(actor);
        for scope in runtime.active_scopes() {
            let dht_name = scoped_provider_host(&scope, mesh.peer_id());
            let reg_span = tracing::info_span!(
                "remote.setup.spawn_provider_host",
                hostname = %hostname,
                dht_name = %dht_name,
                scope = %scope,
            );
            runtime
                .register_actor(actor_ref.clone(), dht_name.clone())
                .instrument(reg_span)
                .await;
            log::info!("ProviderHostActor registered in DHT as '{}'", dht_name);
        }
        Some(actor_ref)
    } else {
        None
    };

    // ── 4. Build a peer-name → addr map for O(1) lookup ──────────────────────

    let peer_map: std::collections::HashMap<&str, &str> = mesh_cfg
        .peers
        .iter()
        .map(|p| (p.name.as_str(), p.addr.as_str()))
        .collect();

    // ── 5. For each [[remote_agents]], look up RemoteNodeManager and register ─

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

        match register_remote_agent(&mesh, remote, peer_addr).await {
            Ok(agent_info) => {
                log::info!(
                    "Registered remote agent '{}' (peer='{}')",
                    agent_info.id,
                    remote.peer
                );
                // Register a unified RemoteAgentHandle that implements
                // the AgentHandle trait for both delegation and event fanout.
                let remote_handle = Arc::new(super::remote_handle::RemoteAgentHandle::new(
                    remote.peer.clone(),
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

    Ok(MeshSetupResult {
        mesh,
        registry,
        provider_host,
    })
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
) -> Result<AgentInfo> {
    use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};

    // Look up the remote node's manager via scoped DHT names.
    // Give up after a short timeout if the peer is not yet discoverable.
    let lookup_timeout = std::time::Duration::from_secs(5);
    let runtime = super::runtime_handle::MeshRuntimeHandle::from(mesh.clone());
    let mut found_ref = None;

    for scope in runtime.active_scopes() {
        let lookup_result = tokio::time::timeout(
            lookup_timeout,
            runtime.lookup_actor::<RemoteNodeManager>(super::scope::scoped_node_manager(&scope)),
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
                    "DHT lookup error for peer '{}' in scope '{}': {}",
                    remote.peer,
                    scope,
                    e
                );
            }
            Err(_timeout) => {}
        }
    }

    match found_ref {
        Some(node_manager_ref) => {
            // Confirm the peer is reachable by calling GetNodeInfo.
            match node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo).await {
                Ok(node_info) => {
                    tracing::Span::current()
                        .record("reachable", true)
                        .record("peer_hostname", &node_info.hostname);
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
            }
        }
        None => {
            tracing::Span::current().record("reachable", false);
            log::debug!(
                "Peer '{}' not yet in scoped DHT; registering remote agent '{}' speculatively",
                remote.peer,
                remote.id
            );
        }
    }

    // Build the AgentInfo regardless — the peer might become available later.
    let info = AgentInfo {
        id: remote.id.clone(),
        name: remote.name.clone(),
        description: remote.description.clone(),
        capabilities: remote.capabilities.clone(),
        required_capabilities: vec![],
        meta: Some(serde_json::json!({
            "remote": true,
            "peer": remote.peer,
        })),
    };

    Ok(info)
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn get_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.is_empty()
    {
        return h;
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
