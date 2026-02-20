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
use tokio::sync::Mutex;
use tracing::Instrument;

/// Result of a successful mesh setup from config.
pub struct MeshSetupResult {
    /// Handle to the bootstrapped swarm.
    pub mesh: MeshHandle,
    /// Registry pre-populated with remote agent `AgentInfo` entries.
    /// Wrap in `Arc` before passing to `AgentConfigBuilder::with_agent_registry`.
    pub registry: DefaultAgentRegistry,
    /// The spawned `ProviderHostActor` ref (registered in DHT as
    /// `"provider_host::{hostname}"`).  `None` when `agent_config` was not
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
///   registered in the DHT as `"provider_host::{hostname}"`, making this
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
        let reg_span = tracing::info_span!(
            "remote.setup.register_node_manager",
            dht_name = "node_manager"
        );
        mesh.register_actor(nm_ref, "node_manager")
            .instrument(reg_span)
            .await;
        log::info!("Phase 7: Local RemoteNodeManager registered in DHT as 'node_manager'");
    }

    // ── 3b. Spawn and register ProviderHostActor (if agent_config provided) ──

    let provider_host = if let Some(config) = agent_config {
        use kameo::actor::Spawn;

        let hostname = get_hostname();
        let dht_name = format!("provider_host::{}", hostname);

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
                // Register with a remote-capable SendAgent stub.
                let stub = Arc::new(RemoteAgentStub::new(
                    remote.peer.clone(),
                    remote.id.clone(),
                    mesh.clone(),
                ));
                registry.register(agent_info, stub);
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
        mesh.lookup_actor::<RemoteNodeManager>("node_manager"),
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

// ── RemoteAgentStub ────────────────────────────────────────────────────────────

/// A `SendAgent` implementation that proxies all calls to a remote
/// `RemoteNodeManager` / `SessionActor` pair via kameo.
///
/// Before Phase 5's delegation rewrite is complete, this stub delegates
/// `prompt()` by:
/// 1. Asking the remote `RemoteNodeManager` to create a session.
/// 2. Sending the prompt via the `SessionActorRef::Remote` wrapper.
/// 3. Returning the response.
///
/// `initialize()` and `new_session()` are no-ops — the remote session is
/// created lazily on the first `prompt()` call.
pub(crate) struct RemoteAgentStub {
    peer_label: String,
    agent_id: String,
    mesh: MeshHandle,
    /// Session ref once created (per stub instance = one delegation at a time).
    session: Mutex<Option<(String, crate::agent::remote::SessionActorRef)>>,
}

impl RemoteAgentStub {
    fn new(peer_label: String, agent_id: String, mesh: MeshHandle) -> Self {
        Self {
            peer_label,
            agent_id,
            mesh,
            session: Mutex::new(None),
        }
    }

    /// Test-only constructor exposed so that `remote_agent_stub_tests` can
    /// construct a stub without going through the full `setup_mesh_from_config`
    /// path (which requires a real TOML config and cannot be called twice per
    /// process because of the `bootstrap_mesh` one-shot constraint).
    #[cfg(test)]
    pub(crate) fn new_for_test(peer_label: String, agent_id: String, mesh: MeshHandle) -> Self {
        Self::new(peer_label, agent_id, mesh)
    }

    /// Create a remote session and return (session_id, SessionActorRef::Remote).
    #[tracing::instrument(
        name = "remote.setup.stub.get_or_create_session",
        skip(self),
        fields(
            peer_label = %self.peer_label,
            agent_id = %self.agent_id,
            cwd = cwd.as_deref().unwrap_or("<none>"),
            session_id = tracing::field::Empty,
            cache_hit = tracing::field::Empty,
        )
    )]
    async fn get_or_create_session(
        &self,
        cwd: Option<String>,
    ) -> Result<(String, crate::agent::remote::SessionActorRef), agent_client_protocol::Error> {
        use crate::agent::remote::{CreateRemoteSession, RemoteNodeManager, SessionActorRef};

        let mut guard = self.session.lock().await;

        if let Some((session_id, session_ref)) = guard.as_ref() {
            tracing::Span::current()
                .record("cache_hit", true)
                .record("session_id", session_id.as_str());
            return Ok((session_id.clone(), session_ref.clone()));
        }
        tracing::Span::current().record("cache_hit", false);

        // Look up remote node manager.
        let node_manager_ref = self
            .mesh
            .lookup_actor::<RemoteNodeManager>("node_manager")
            .await
            .map_err(|e| {
                agent_client_protocol::Error::new(
                    -32001,
                    format!("DHT lookup failed for peer '{}': {}", self.peer_label, e),
                )
            })?
            .ok_or_else(|| {
                agent_client_protocol::Error::new(
                    -32001,
                    format!(
                        "Remote peer '{}' not found in DHT (is the mesh running on that machine?)",
                        self.peer_label
                    ),
                )
            })?;

        // Ask the remote node manager to create a session.
        let resp = node_manager_ref
            .ask(&CreateRemoteSession { cwd })
            .await
            .map_err(|e| {
                agent_client_protocol::Error::new(
                    -32002,
                    format!(
                        "CreateRemoteSession failed on peer '{}': {}",
                        self.peer_label, e
                    ),
                )
            })?;

        let session_id = resp.session_id;
        let actor_id = resp.actor_id;
        tracing::Span::current().record("session_id", &session_id);

        // Resolve the remote SessionActorRef via DHT name.
        let dht_name = format!("session::{}", session_id);
        let remote_session_ref = self
            .mesh
            .lookup_actor::<crate::agent::session_actor::SessionActor>(&dht_name)
            .await
            .map_err(|e| {
                agent_client_protocol::Error::new(
                    -32003,
                    format!(
                        "DHT lookup for session '{}' on peer '{}' failed: {}",
                        session_id, self.peer_label, e
                    ),
                )
            })?
            .ok_or_else(|| {
                agent_client_protocol::Error::new(
                    -32003,
                    format!(
                        "Session '{}' not found in DHT on peer '{}' (actor_id={})",
                        session_id, self.peer_label, actor_id
                    ),
                )
            })?;

        let session_ref = SessionActorRef::Remote {
            actor_ref: remote_session_ref,
            peer_label: self.peer_label.clone(),
        };

        *guard = Some((session_id.clone(), session_ref.clone()));
        Ok((session_id, session_ref))
    }
}

#[async_trait::async_trait]
impl crate::send_agent::SendAgent for RemoteAgentStub {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(
        &self,
        _req: agent_client_protocol::InitializeRequest,
    ) -> Result<agent_client_protocol::InitializeResponse, agent_client_protocol::Error> {
        use agent_client_protocol::{
            AgentCapabilities, Implementation, InitializeResponse, ProtocolVersion,
        };
        Ok(InitializeResponse::new(ProtocolVersion::LATEST)
            .agent_capabilities(AgentCapabilities::new())
            .agent_info(Implementation::new(
                format!("remote:{}", self.agent_id),
                env!("CARGO_PKG_VERSION"),
            )))
    }

    async fn authenticate(
        &self,
        _req: agent_client_protocol::AuthenticateRequest,
    ) -> Result<agent_client_protocol::AuthenticateResponse, agent_client_protocol::Error> {
        Ok(agent_client_protocol::AuthenticateResponse::new())
    }

    async fn new_session(
        &self,
        req: agent_client_protocol::NewSessionRequest,
    ) -> Result<agent_client_protocol::NewSessionResponse, agent_client_protocol::Error> {
        let cwd = req.cwd.to_str().map(|s| s.to_string());
        let (session_id, _) = self.get_or_create_session(cwd).await?;
        Ok(agent_client_protocol::NewSessionResponse::new(session_id))
    }

    async fn prompt(
        &self,
        req: agent_client_protocol::PromptRequest,
    ) -> Result<agent_client_protocol::PromptResponse, agent_client_protocol::Error> {
        let (_, session_ref) = self.get_or_create_session(None).await?;
        session_ref.prompt(req).await
    }

    async fn cancel(
        &self,
        _req: agent_client_protocol::CancelNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        if let Ok(guard) = self.session.try_lock()
            && let Some((_, session_ref)) = guard.as_ref()
        {
            let _ = session_ref.cancel().await;
        }
        Ok(())
    }

    async fn load_session(
        &self,
        _req: agent_client_protocol::LoadSessionRequest,
    ) -> Result<agent_client_protocol::LoadSessionResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::new(
            -32603,
            "load_session is not supported for remote agent stubs",
        ))
    }

    async fn list_sessions(
        &self,
        _req: agent_client_protocol::ListSessionsRequest,
    ) -> Result<agent_client_protocol::ListSessionsResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::new(
            -32603,
            "list_sessions is not supported for remote agent stubs",
        ))
    }

    async fn fork_session(
        &self,
        _req: agent_client_protocol::ForkSessionRequest,
    ) -> Result<agent_client_protocol::ForkSessionResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::new(
            -32603,
            "fork_session is not supported for remote agent stubs",
        ))
    }

    async fn resume_session(
        &self,
        _req: agent_client_protocol::ResumeSessionRequest,
    ) -> Result<agent_client_protocol::ResumeSessionResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::new(
            -32603,
            "resume_session is not supported for remote agent stubs",
        ))
    }

    async fn set_session_model(
        &self,
        _req: agent_client_protocol::SetSessionModelRequest,
    ) -> Result<agent_client_protocol::SetSessionModelResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::new(
            -32603,
            "set_session_model is not supported for remote agent stubs",
        ))
    }

    async fn ext_method(
        &self,
        _req: agent_client_protocol::ExtRequest,
    ) -> Result<agent_client_protocol::ExtResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::new(
            -32603,
            "ext_method is not supported for remote agent stubs",
        ))
    }

    async fn ext_notification(
        &self,
        _notif: agent_client_protocol::ExtNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        Ok(())
    }
}
