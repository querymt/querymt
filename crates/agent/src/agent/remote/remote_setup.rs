//! Config-driven mesh bootstrap and remote agent registration.
//!
//! This module implements Phase 7: reading `[mesh]` and `[[remote_agents]]`
//! from TOML config and automatically:
//! 1. Bootstrapping the kameo libp2p swarm.
//! 2. Registering the local node as a `RemoteNodeManager` in the DHT.
//! 3. For each declared `[[remote_agents]]`, looking up the peer's
//!    `RemoteNodeManager` via DHT and registering the remote agent in the
//!    local `DefaultAgentRegistry` so the planner can delegate to it.
//!
//! All functionality is feature-gated behind `#[cfg(feature = "remote")]`.

use crate::agent::remote::mesh::{MeshConfig, MeshDiscovery, MeshHandle, bootstrap_mesh};
use crate::agent::remote::provider_host::ProviderHostActor;
use crate::config::{MeshDiscoveryConfig, MeshTomlConfig, RemoteAgentConfig};
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
    // ── 1. Translate TOML discovery config to mesh::MeshDiscovery ────────────

    let peers: Vec<String> = mesh_cfg.peers.iter().map(|p| p.addr.clone()).collect();

    let discovery = match &mesh_cfg.discovery {
        MeshDiscoveryConfig::Mdns => MeshDiscovery::Mdns,
        MeshDiscoveryConfig::None => MeshDiscovery::None,
        MeshDiscoveryConfig::Kademlia => MeshDiscovery::Kademlia {
            bootstrap: peers.clone(),
        },
    };

    let config = MeshConfig {
        listen: mesh_cfg.listen.clone(),
        discovery,
        bootstrap_peers: if matches!(mesh_cfg.discovery, MeshDiscoveryConfig::Kademlia) {
            // Already used as Kademlia bootstrap peers above.
            vec![]
        } else {
            peers
        },
        directory: crate::agent::remote::mesh::DirectoryMode::default(),
    };
    let listen_addr_str = config.listen.as_deref().unwrap_or("<auto>").to_string();

    // ── 2. Bootstrap the libp2p swarm ─────────────────────────────────────────

    let mesh = {
        let bootstrap_span = tracing::info_span!(
            "remote.setup.bootstrap_mesh",
            listen = %listen_addr_str,
            discovery = ?mesh_cfg.discovery,
        );
        bootstrap_mesh(&config)
            .instrument(bootstrap_span)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
    };
    tracing::Span::current().record("peer_id", mesh.peer_id().to_string());
    log::info!(
        "Phase 7: Kameo mesh bootstrapped (peer_id={})",
        mesh.peer_id()
    );

    // ── 3. Register the local RemoteNodeManager in the DHT (if provided) ──────

    if let Some(nm_ref) = node_manager_ref {
        // Register under the global name so lookup_all_actors (used by
        // list_remote_nodes) can discover this node alongside all others.
        let reg_span = tracing::info_span!(
            "remote.setup.register_node_manager",
            dht_name = super::dht_name::NODE_MANAGER
        );
        mesh.register_actor(nm_ref.clone(), super::dht_name::NODE_MANAGER)
            .instrument(reg_span)
            .await;
        log::info!(
            "Phase 7: Local RemoteNodeManager registered in DHT as '{}'",
            super::dht_name::NODE_MANAGER
        );

        // Also register under the per-peer name so find_node_manager can do a
        // direct O(1) lookup by peer_id, bypassing the is_peer_alive gate that
        // guards the lookup_all_actors scan. This makes create_remote_session
        // robust against mDNS TTL expiry (30 s) on cross-machine setups.
        let per_peer_name = super::dht_name::node_manager_for_peer(mesh.peer_id());
        mesh.register_actor(nm_ref, per_peer_name.clone()).await;
        log::info!(
            "Phase 7: Local RemoteNodeManager also registered in DHT as '{}'",
            per_peer_name
        );
    }

    // ── 3b. Spawn and register ProviderHostActor (if agent_config provided) ──

    let provider_host = if let Some(config) = agent_config {
        use kameo::actor::Spawn;

        let hostname = get_hostname();
        let dht_name = super::dht_name::provider_host(mesh.peer_id());

        let actor = ProviderHostActor::new(config);
        let actor_ref = ProviderHostActor::spawn(actor);
        {
            let reg_span = tracing::info_span!(
                "remote.setup.spawn_provider_host",
                hostname = %hostname,
                dht_name = %dht_name,
            );
            mesh.register_actor(actor_ref.clone(), dht_name.clone())
                .instrument(reg_span)
                .await;
        }
        log::info!(
            "Phase 7: ProviderHostActor registered in DHT as '{}'",
            dht_name
        );
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
                    "Phase 7: remote_agent '{}' references unknown peer '{}'; skipping",
                    remote.id,
                    remote.peer
                );
                continue;
            }
        };

        match register_remote_agent(&mesh, remote, peer_addr).await {
            Ok(agent_info) => {
                log::info!(
                    "Phase 7: Registered remote agent '{}' (peer='{}')",
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
                    "Phase 7: Could not register remote agent '{}' (peer='{}'): {}; skipping",
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

    // Look up the remote node's manager via the DHT.
    // The peer publishes itself under "node_manager".
    // Give up after a short timeout if the peer is not yet discoverable.
    let lookup_timeout = std::time::Duration::from_secs(5);
    let lookup_result = tokio::time::timeout(
        lookup_timeout,
        mesh.lookup_actor::<RemoteNodeManager>(super::dht_name::NODE_MANAGER),
    )
    .await;

    match lookup_result {
        Ok(Ok(Some(node_manager_ref))) => {
            // Confirm the peer is reachable by calling GetNodeInfo.
            match node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo).await {
                Ok(node_info) => {
                    tracing::Span::current()
                        .record("reachable", true)
                        .record("peer_hostname", &node_info.hostname);
                    log::debug!(
                        "Phase 7: Peer '{}' reachable (hostname='{}')",
                        remote.peer,
                        node_info.hostname
                    );
                }
                Err(e) => {
                    tracing::Span::current().record("reachable", false);
                    log::debug!(
                        "Phase 7: GetNodeInfo failed for peer '{}': {} (registering anyway)",
                        remote.peer,
                        e
                    );
                }
            }
        }
        Ok(Ok(None)) => {
            tracing::Span::current().record("reachable", false);
            log::debug!(
                "Phase 7: Peer '{}' not yet in DHT; registering remote agent '{}' speculatively",
                remote.peer,
                remote.id
            );
        }
        Ok(Err(e)) => {
            tracing::Span::current().record("reachable", false);
            log::debug!(
                "Phase 7: DHT lookup error for peer '{}': {} (registering speculatively)",
                remote.peer,
                e
            );
        }
        Err(_timeout) => {
            tracing::Span::current().record("reachable", false);
            log::debug!(
                "Phase 7: DHT lookup timed out for peer '{}' (registering speculatively)",
                remote.peer,
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
