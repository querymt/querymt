use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};

use super::events::{
    handle_connection_closed, handle_connection_established, handle_mdns_discovered,
    handle_mdns_expired, log_kameo_messaging_event, peer_id_from_multiaddr,
    reconnect_backoff_duration, refresh_mesh_state_known_peers,
};
use super::handle::ReRegisterFn;
use super::{
    MeshConfig, MeshDiscovery, MeshError, MeshEvent, MeshHandle, MeshScopeId, MeshTransportKind,
    MeshTransportMode, RouteTable, SwarmCommand,
};
use libp2p::{Multiaddr, PeerId};

/// Shared pre-bootstrap setup: load identity, validate peers, create channels.
pub(super) struct MeshBootstrapContext {
    pub(super) keypair: libp2p::identity::Keypair,
    pub(super) peer_events_tx: broadcast::Sender<MeshEvent>,
    pub(super) routes: Arc<RouteTable>,
    pub(super) known_peers: Arc<RwLock<HashMap<PeerId, HashSet<Multiaddr>>>>,
    pub(super) re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
    pub(super) local_hostname: String,
}

pub(super) fn prepare_bootstrap(config: &MeshConfig) -> Result<MeshBootstrapContext, MeshError> {
    let keypair = super::super::identity::load_or_generate_keypair(config.identity_file.as_deref())
        .map_err(|e| MeshError::SwarmError(format!("failed to load mesh identity: {e}")))?;

    for peer_addr in &config.bootstrap_peers {
        peer_addr
            .parse::<libp2p::Multiaddr>()
            .map_err(|e| MeshError::InvalidBootstrapAddr {
                addr: peer_addr.clone(),
                reason: e.to_string(),
            })?;
    }

    let (peer_events_tx, _) = broadcast::channel::<MeshEvent>(64);
    let routes = Arc::new(RouteTable::new(Duration::from_secs(90)));
    let known_peers = Arc::new(RwLock::new(HashMap::new()));
    let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
    let local_hostname = super::resolve_local_hostname();

    Ok(MeshBootstrapContext {
        keypair,
        peer_events_tx,
        routes,
        known_peers,
        re_register_fns,
        local_hostname,
    })
}

pub(super) fn finalize_bootstrap(
    local_peer_id: PeerId,
    ctx: MeshBootstrapContext,
    listen_label: &str,
    transport_mode: MeshTransportMode,
    swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    stream_reconnect_grace: std::time::Duration,
) -> MeshHandle {
    log::info!(
        "Kameo mesh bootstrapped: peer_id={}, listen={}",
        local_peer_id,
        listen_label,
    );

    let invite_store = match super::super::invite::default_invite_store_path() {
        Ok(path) => match super::super::invite::InviteStore::load_or_create(&path) {
            Ok(store) => Some(Arc::new(RwLock::new(store))),
            Err(e) => {
                log::warn!("Failed to load invite store: {e}; invites will not be persisted");
                None
            }
        },
        Err(e) => {
            log::warn!("Cannot determine invite store path: {e}; invites will not be persisted");
            None
        }
    };

    let mesh_state_store = match super::super::mesh_state::default_mesh_state_path() {
        Ok(path) => match super::super::mesh_state::MeshStateStore::load_or_create(&path) {
            Ok(store) => Some(Arc::new(RwLock::new(store))),
            Err(e) => {
                log::warn!(
                    "Failed to load mesh state store: {e}; iroh state will not be persisted"
                );
                None
            }
        },
        Err(e) => {
            log::warn!("Cannot determine mesh state path: {e}; iroh state will not be persisted");
            None
        }
    };

    MeshHandle::new(
        local_peer_id,
        ctx.peer_events_tx,
        ctx.routes,
        ctx.local_hostname,
        ctx.re_register_fns,
        ctx.keypair,
        invite_store,
        mesh_state_store,
        transport_mode,
        swarm_cmd_tx,
        stream_reconnect_grace,
    )
}

pub(super) async fn bootstrap_lan_mesh(config: &MeshConfig) -> Result<MeshHandle, MeshError> {
    use futures_util::StreamExt as _;
    use kameo::remote;
    use libp2p::{
        SwarmBuilder, mdns, noise,
        swarm::{NetworkBehaviour, SwarmEvent},
        tcp, yamux,
    };

    let ctx = prepare_bootstrap(config)?;
    let listen_addr = config.listen.as_deref().unwrap_or("/ip4/0.0.0.0/tcp/0");

    let peer_events_tx_loop = ctx.peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&ctx.known_peers);
    let routes_loop = Arc::clone(&ctx.routes);
    let re_register_fns_loop = Arc::clone(&ctx.re_register_fns);

    #[derive(NetworkBehaviour)]
    struct MeshBehaviour {
        kameo: remote::Behaviour,
        mdns: mdns::tokio::Behaviour,
    }

    let mut swarm = SwarmBuilder::with_existing_identity(ctx.keypair.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|e| MeshError::SwarmError(e.to_string()))?
        .with_quic()
        .with_behaviour(|key| {
            let local_peer_id = key.public().to_peer_id();
            let kameo_behaviour = remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default()
                    .with_request_timeout(config.request_timeout)
                    .with_response_size_maximum(50 * 1024 * 1024),
            );
            let mdns_config = mdns::Config {
                ttl: std::time::Duration::from_secs(30),
                query_interval: std::time::Duration::from_secs(15),
                ..mdns::Config::default()
            };
            let mdns_behaviour = mdns::tokio::Behaviour::new(mdns_config, local_peer_id)?;
            Ok(MeshBehaviour {
                kameo: kameo_behaviour,
                mdns: mdns_behaviour,
            })
        })
        .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(300)))
        .build();

    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    swarm
        .listen_on(listen_addr.parse().map_err(|e: libp2p::multiaddr::Error| {
            MeshError::InvalidListenAddr {
                addr: listen_addr.to_string(),
                reason: e.to_string(),
            }
        })?)
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    for peer_addr in &config.bootstrap_peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated above");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer: {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    let local_peer_id = *swarm.local_peer_id();
    let (swarm_cmd_tx_lan, _swarm_cmd_rx_lan) = mpsc::unbounded_channel::<SwarmCommand>();

    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(MeshBehaviourEvent::Kameo(remote::Event::Messaging(
                    event,
                ))) => {
                    log_kameo_messaging_event(&event);
                }
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    handle_mdns_discovered(
                        &mut swarm,
                        list,
                        &known_peers_loop,
                        &routes_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
                    );
                }
                SwarmEvent::Behaviour(MeshBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    handle_mdns_expired(
                        &mut swarm,
                        list,
                        &known_peers_loop,
                        &routes_loop,
                        &peer_events_tx_loop,
                    );
                }
                SwarmEvent::ConnectionEstablished {
                    peer_id, endpoint, ..
                } => {
                    handle_connection_established(
                        &mut swarm,
                        peer_id,
                        endpoint.get_remote_address().clone(),
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
                        MeshTransportKind::Lan,
                        MeshScopeId::lan_default(),
                        100,
                    );
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    num_established,
                    ..
                } => {
                    handle_connection_closed(
                        peer_id,
                        num_established,
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                    );
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    log::warn!("Outgoing connection error (peer={:?}): {}", peer_id, error);
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    log::info!("ActorSwarm listening on {address}");
                }
                _ => {}
            }
        }
    });

    if matches!(&config.discovery, MeshDiscovery::Kademlia { bootstrap } if !bootstrap.is_empty())
        && let MeshDiscovery::Kademlia { bootstrap } = &config.discovery
    {
        for addr in bootstrap {
            log::info!("Kademlia bootstrap peer: {}", addr);
        }
    }

    Ok(finalize_bootstrap(
        local_peer_id,
        ctx,
        listen_addr,
        MeshTransportMode::Lan,
        swarm_cmd_tx_lan,
        config.stream_reconnect_grace,
    ))
}

pub(super) async fn bootstrap_iroh_mesh(config: &MeshConfig) -> Result<MeshHandle, MeshError> {
    use futures_util::StreamExt as _;
    use kameo::remote;
    use libp2p::swarm::{NetworkBehaviour, Swarm, SwarmEvent};

    let ctx = prepare_bootstrap(config)?;

    let peer_events_tx_loop = ctx.peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&ctx.known_peers);
    let routes_loop = Arc::clone(&ctx.routes);
    let re_register_fns_loop = Arc::clone(&ctx.re_register_fns);

    let local_mesh_id = config.invite.as_ref().map(|invite| {
        super::super::invite::mesh_id_for(
            &invite.grant.inviter_peer_id,
            invite.grant.mesh_name.as_deref(),
        )
    });

    let mesh_state_store_loop: Option<Arc<RwLock<super::super::mesh_state::MeshStateStore>>> =
        super::super::mesh_state::default_mesh_state_path()
            .ok()
            .and_then(|p| super::super::mesh_state::MeshStateStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)));

    let iroh_config = libp2p_iroh::TransportConfig {
        timeout: config.request_timeout,
        ..Default::default()
    };

    let transport = libp2p_iroh::Transport::with_config(Some(&ctx.keypair), iroh_config)
        .await
        .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

    let local_peer_id = transport.peer_id;

    #[derive(NetworkBehaviour)]
    struct IrohMeshBehaviour {
        kameo: remote::Behaviour,
    }

    let kameo_behaviour = remote::Behaviour::new(
        local_peer_id,
        remote::messaging::Config::default()
            .with_request_timeout(config.request_timeout)
            .with_response_size_maximum(50 * 1024 * 1024),
    );

    let behaviour = IrohMeshBehaviour {
        kameo: kameo_behaviour,
    };

    let mut swarm = Swarm::new(
        libp2p::Transport::boxed(transport),
        behaviour,
        local_peer_id,
        libp2p::swarm::Config::with_executor(Box::new(|fut| {
            tokio::spawn(fut);
        }))
        .with_idle_connection_timeout(std::time::Duration::from_secs(300)),
    );

    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    swarm
        .listen_on(Multiaddr::empty())
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    if let Some(ref invite) = config.invite {
        let inviter_addr: Multiaddr = format!("/p2p/{}", invite.grant.inviter_peer_id)
            .parse()
            .map_err(|e: libp2p::multiaddr::Error| {
                MeshError::SwarmError(format!(
                    "invalid inviter PeerId '{}': {}",
                    invite.grant.inviter_peer_id, e
                ))
            })?;
        match swarm.dial(inviter_addr.clone()) {
            Ok(_) => log::info!(
                "Dialing inviter via iroh relay: {} (mesh: {:?})",
                inviter_addr,
                invite.grant.mesh_name
            ),
            Err(e) => log::warn!("Failed to dial inviter {}: {}", inviter_addr, e),
        }
    }

    for peer_addr in &config.bootstrap_peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated in prepare_bootstrap");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer (iroh): {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    let mut reconnect_targets: HashSet<PeerId> = HashSet::new();

    if let Some(ref invite) = config.invite
        && let Ok(inviter_pid) = invite.grant.inviter_peer_id.parse::<PeerId>()
    {
        reconnect_targets.insert(inviter_pid);
    }

    for peer_addr in &config.bootstrap_peers {
        if let Ok(addr) = peer_addr.parse::<Multiaddr>()
            && let Some(peer_id) = peer_id_from_multiaddr(&addr)
        {
            reconnect_targets.insert(peer_id);
        }
    }

    if let Some(ref ms) = mesh_state_store_loop {
        let store = ms.read();
        if let Some(mesh_id) = local_mesh_id.as_deref() {
            for peer in store.reconnect_peers_for_mesh(mesh_id) {
                if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                    reconnect_targets.insert(pid);
                }
            }
        } else {
            for (_, peer) in store.all_reconnect_peers() {
                if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                    reconnect_targets.insert(pid);
                }
            }
        }
    }

    reconnect_targets.remove(&local_peer_id);

    let (swarm_cmd_tx_iroh, mut swarm_cmd_rx_iroh) = mpsc::unbounded_channel::<SwarmCommand>();

    tokio::spawn(async move {
        let mut pending_dials: HashSet<PeerId> = HashSet::new();
        let mut reconnect_attempts: HashMap<PeerId, u32> = HashMap::new();
        let mut reconnect_next_due: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
        let mut reconnect_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        reconnect_tick.tick().await;

        loop {
            tokio::select! {
                _ = reconnect_tick.tick() => {
                    let now = tokio::time::Instant::now();
                    for peer_id in reconnect_targets.iter().copied().collect::<Vec<_>>() {
                        if peer_id == local_peer_id {
                            continue;
                        }
                        if routes_loop.is_peer_alive(&peer_id) || pending_dials.contains(&peer_id) {
                            continue;
                        }
                        if reconnect_next_due
                            .get(&peer_id)
                            .is_some_and(|due| *due > now)
                        {
                            continue;
                        }

                        let addr: Multiaddr = format!("/p2p/{peer_id}")
                            .parse()
                            .expect("PeerId always produces a valid /p2p/ multiaddr");
                        match swarm.dial(addr) {
                            Ok(_) => {
                                log::debug!("Reconnect dial (iroh): {}", peer_id);
                                pending_dials.insert(peer_id);
                            }
                            Err(e) => {
                                let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                *attempt = attempt.saturating_add(1);
                                let delay = reconnect_backoff_duration(*attempt);
                                reconnect_next_due.insert(peer_id, now + delay);
                                log::warn!(
                                    "Reconnect dial failed (iroh, peer={}, attempt={}): {}",
                                    peer_id,
                                    *attempt,
                                    e
                                );
                            }
                        }
                    }
                }
                Some(cmd) = swarm_cmd_rx_iroh.recv() => {
                    match cmd {
                        SwarmCommand::DialPeer { peer_id, .. } => {
                            reconnect_targets.insert(peer_id);
                            if pending_dials.contains(&peer_id) || routes_loop.is_peer_alive(&peer_id) {
                                log::debug!("Skipping dial for {} (already connected or pending)", peer_id);
                                continue;
                            }
                            let addr: Multiaddr = format!("/p2p/{peer_id}")
                                .parse()
                                .expect("PeerId always produces a valid /p2p/ multiaddr");
                            match swarm.dial(addr) {
                                Ok(_) => {
                                    log::info!("Dialing peer (iroh): {}", peer_id);
                                    pending_dials.insert(peer_id);
                                }
                                Err(e) => {
                                    let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                    *attempt = attempt.saturating_add(1);
                                    reconnect_next_due.insert(
                                        peer_id,
                                        tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                                    );
                                    log::warn!("Failed to dial peer {} (iroh): {}", peer_id, e);
                                }
                            }
                        }
                        SwarmCommand::JoinIrohScope { peers, .. } => {
                            for peer_id in peers {
                                reconnect_targets.insert(peer_id);
                            }
                        }
                        SwarmCommand::LeaveIrohScope { .. } => {}
                    }
                }
                event = swarm.select_next_some() => {
                match event {
                SwarmEvent::Behaviour(IrohMeshBehaviourEvent::Kameo(remote::Event::Messaging(event))) => {
                    log_kameo_messaging_event(&event);
                }
                    SwarmEvent::ConnectionEstablished {
                        peer_id, endpoint, ..
                    } => {
                        pending_dials.remove(&peer_id);
                        reconnect_targets.insert(peer_id);
                        reconnect_attempts.remove(&peer_id);
                        reconnect_next_due.remove(&peer_id);

                    let (scope, priority) = if let Some(mesh_id) = local_mesh_id.clone() {
                        (MeshScopeId::Iroh { mesh_id }, 70)
                    } else {
                        (MeshScopeId::lan_default(), 100)
                    };
                    handle_connection_established(
                        &mut swarm,
                        peer_id,
                        endpoint.get_remote_address().clone(),
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                        &re_register_fns_loop,
                        MeshTransportKind::Iroh,
                        scope,
                        priority,
                    );

                    refresh_mesh_state_known_peers(&mesh_state_store_loop, &routes_loop);
                }
                    SwarmEvent::ConnectionClosed {
                        peer_id,
                        num_established,
                        ..
                    } => {
                        reconnect_targets.insert(peer_id);
                        reconnect_next_due.remove(&peer_id);

                    handle_connection_closed(
                        peer_id,
                        num_established,
                        &routes_loop,
                        &known_peers_loop,
                        &peer_events_tx_loop,
                    );
                }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        if let Some(pid) = peer_id {
                            pending_dials.remove(&pid);
                            reconnect_targets.insert(pid);
                            let attempt = reconnect_attempts.entry(pid).or_insert(0);
                            *attempt = attempt.saturating_add(1);
                            reconnect_next_due.insert(
                                pid,
                                tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                            );
                        }

                    log::warn!(
                        "Outgoing connection error (iroh, peer={:?}): {}",
                        peer_id,
                        error
                    );
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    log::info!("ActorSwarm listening on {} (iroh)", address);
                }
                _ => {}
            }
            }
            }
        }
    });

    Ok(finalize_bootstrap(
        local_peer_id,
        ctx,
        "iroh-relay",
        MeshTransportMode::Iroh,
        swarm_cmd_tx_iroh,
        config.stream_reconnect_grace,
    ))
}

pub(super) async fn bootstrap_composite_mesh(config: &MeshConfig) -> Result<MeshHandle, MeshError> {
    use futures_util::StreamExt as _;
    use kameo::remote;
    use libp2p::{
        SwarmBuilder, mdns, noise,
        swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle},
        tcp, yamux,
    };

    let ctx = prepare_bootstrap(config)?;
    let listen_addr = config.listen.as_deref().unwrap_or("/ip4/0.0.0.0/tcp/0");

    let peer_events_tx_loop = ctx.peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&ctx.known_peers);
    let routes_loop = Arc::clone(&ctx.routes);
    let re_register_fns_loop = Arc::clone(&ctx.re_register_fns);

    let iroh_config = libp2p_iroh::TransportConfig {
        timeout: config.request_timeout,
        ..Default::default()
    };
    let iroh_transport = libp2p_iroh::Transport::with_config(Some(&ctx.keypair), iroh_config)
        .await
        .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

    let enable_mdns = matches!(config.discovery, MeshDiscovery::Mdns);

    #[derive(NetworkBehaviour)]
    struct CompositeMeshBehaviour {
        kameo: remote::Behaviour,
        mdns: Toggle<mdns::tokio::Behaviour>,
    }

    let mut swarm = SwarmBuilder::with_existing_identity(ctx.keypair.clone())
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .map_err(|e| MeshError::SwarmError(e.to_string()))?
        .with_quic()
        .with_other_transport(move |_| iroh_transport)
        .map_err(|e| -> MeshError { match e {} })?
        .with_behaviour(|key| {
            let local_peer_id = key.public().to_peer_id();
            let kameo_behaviour = remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default()
                    .with_request_timeout(config.request_timeout)
                    .with_response_size_maximum(50 * 1024 * 1024),
            );

            let mdns_behaviour = if enable_mdns {
                let mdns_config = mdns::Config {
                    ttl: std::time::Duration::from_secs(30),
                    query_interval: std::time::Duration::from_secs(15),
                    ..mdns::Config::default()
                };
                Some(mdns::tokio::Behaviour::new(mdns_config, local_peer_id)?)
            } else {
                None
            };

            Ok(CompositeMeshBehaviour {
                kameo: kameo_behaviour,
                mdns: mdns_behaviour.into(),
            })
        })
        .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(300)))
        .build();

    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    swarm
        .listen_on(listen_addr.parse().map_err(|e: libp2p::multiaddr::Error| {
            MeshError::InvalidListenAddr {
                addr: listen_addr.to_string(),
                reason: e.to_string(),
            }
        })?)
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    swarm
        .listen_on(Multiaddr::empty())
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    for peer_addr in &config.bootstrap_peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated above");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer: {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    if let Some(ref invite) = config.invite {
        let inviter_addr: Multiaddr = format!("/p2p/{}", invite.grant.inviter_peer_id)
            .parse()
            .map_err(|e: libp2p::multiaddr::Error| {
                MeshError::SwarmError(format!(
                    "invalid inviter PeerId '{}': {}",
                    invite.grant.inviter_peer_id, e
                ))
            })?;
        match swarm.dial(inviter_addr.clone()) {
            Ok(_) => log::info!(
                "Dialing inviter via iroh relay: {} (mesh: {:?})",
                inviter_addr,
                invite.grant.mesh_name
            ),
            Err(e) => log::warn!("Failed to dial inviter {}: {}", inviter_addr, e),
        }
    }

    let local_peer_id = *swarm.local_peer_id();

    let local_mesh_id = config.invite.as_ref().map(|invite| {
        super::super::invite::mesh_id_for(
            &invite.grant.inviter_peer_id,
            invite.grant.mesh_name.as_deref(),
        )
    });

    let mesh_state_store_loop: Option<Arc<RwLock<super::super::mesh_state::MeshStateStore>>> =
        super::super::mesh_state::default_mesh_state_path()
            .ok()
            .and_then(|p| super::super::mesh_state::MeshStateStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)));

    let mut reconnect_targets: HashSet<PeerId> = HashSet::new();

    if let Some(ref invite) = config.invite
        && let Ok(inviter_pid) = invite.grant.inviter_peer_id.parse::<PeerId>()
    {
        reconnect_targets.insert(inviter_pid);
    }

    for peer_addr in &config.bootstrap_peers {
        if let Ok(addr) = peer_addr.parse::<Multiaddr>()
            && let Some(peer_id) = peer_id_from_multiaddr(&addr)
        {
            reconnect_targets.insert(peer_id);
        }
    }

    if let Some(ref ms) = mesh_state_store_loop {
        let store = ms.read();
        if let Some(mesh_id) = local_mesh_id.as_deref() {
            for peer in store.reconnect_peers_for_mesh(mesh_id) {
                if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                    reconnect_targets.insert(pid);
                }
            }
        } else {
            for (_, peer) in store.all_reconnect_peers() {
                if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                    reconnect_targets.insert(pid);
                }
            }
        }
    }

    reconnect_targets.remove(&local_peer_id);

    let (swarm_cmd_tx, mut swarm_cmd_rx) = mpsc::unbounded_channel::<SwarmCommand>();

    tokio::spawn(async move {
        let mut pending_dials: HashSet<PeerId> = HashSet::new();
        let mut reconnect_attempts: HashMap<PeerId, u32> = HashMap::new();
        let mut reconnect_next_due: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
        let mut reconnect_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        reconnect_tick.tick().await;

        loop {
            tokio::select! {
                _ = reconnect_tick.tick() => {
                    let now = tokio::time::Instant::now();
                    for peer_id in reconnect_targets.iter().copied().collect::<Vec<_>>() {
                        if peer_id == local_peer_id {
                            continue;
                        }
                        if routes_loop.is_peer_alive(&peer_id) || pending_dials.contains(&peer_id) {
                            continue;
                        }
                        if reconnect_next_due
                            .get(&peer_id)
                            .is_some_and(|due| *due > now)
                        {
                            continue;
                        }

                        let addr: Multiaddr = format!("/p2p/{peer_id}")
                            .parse()
                            .expect("PeerId always produces a valid /p2p/ multiaddr");
                        match swarm.dial(addr) {
                            Ok(_) => {
                                log::debug!("Reconnect dial (composite): {}", peer_id);
                                pending_dials.insert(peer_id);
                            }
                            Err(e) => {
                                let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                *attempt = attempt.saturating_add(1);
                                let delay = reconnect_backoff_duration(*attempt);
                                reconnect_next_due.insert(peer_id, now + delay);
                                log::warn!(
                                    "Reconnect dial failed (composite, peer={}, attempt={}): {}",
                                    peer_id,
                                    *attempt,
                                    e
                                );
                            }
                        }
                    }
                }
                Some(cmd) = swarm_cmd_rx.recv() => {
                    match cmd {
                        SwarmCommand::DialPeer { peer_id, .. } => {
                            reconnect_targets.insert(peer_id);
                            if pending_dials.contains(&peer_id) || routes_loop.is_peer_alive(&peer_id) {
                                log::debug!("Skipping dial for {} (already connected or pending)", peer_id);
                                continue;
                            }
                            let addr: Multiaddr = format!("/p2p/{peer_id}")
                                .parse()
                                .expect("PeerId always produces a valid /p2p/ multiaddr");
                            match swarm.dial(addr) {
                                Ok(_) => {
                                    log::info!("Dialing peer (composite): {}", peer_id);
                                    pending_dials.insert(peer_id);
                                }
                                Err(e) => {
                                    let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                    *attempt = attempt.saturating_add(1);
                                    reconnect_next_due.insert(
                                        peer_id,
                                        tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                                    );
                                    log::warn!("Failed to dial peer {} (composite): {}", peer_id, e);
                                }
                            }
                        }
                        SwarmCommand::JoinIrohScope { peers, .. } => {
                            for peer_id in peers {
                                reconnect_targets.insert(peer_id);
                            }
                        }
                        SwarmCommand::LeaveIrohScope { .. } => {}
                    }
                }
                event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(CompositeMeshBehaviourEvent::Kameo(remote::Event::Messaging(event))) => {
                        log_kameo_messaging_event(&event);
                    }
                    SwarmEvent::Behaviour(CompositeMeshBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                        handle_mdns_discovered(
                            &mut swarm,
                            list,
                            &known_peers_loop,
                            &routes_loop,
                            &peer_events_tx_loop,
                            &re_register_fns_loop,
                        );
                    }
                    SwarmEvent::Behaviour(CompositeMeshBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                        handle_mdns_expired(
                            &mut swarm,
                            list,
                            &known_peers_loop,
                            &routes_loop,
                            &peer_events_tx_loop,
                        );
                    }
                    SwarmEvent::ConnectionEstablished {
                        peer_id, endpoint, ..
                    } => {
                        pending_dials.remove(&peer_id);
                        reconnect_targets.insert(peer_id);
                        reconnect_attempts.remove(&peer_id);
                        reconnect_next_due.remove(&peer_id);

                        handle_connection_established(
                            &mut swarm,
                            peer_id,
                            endpoint.get_remote_address().clone(),
                            &routes_loop,
                            &known_peers_loop,
                            &peer_events_tx_loop,
                            &re_register_fns_loop,
                            MeshTransportKind::Lan,
                            MeshScopeId::lan_default(),
                            100,
                        );

                        refresh_mesh_state_known_peers(&mesh_state_store_loop, &routes_loop);
                    }
                    SwarmEvent::ConnectionClosed {
                        peer_id,
                        num_established,
                        ..
                    } => {
                        reconnect_targets.insert(peer_id);
                        reconnect_next_due.remove(&peer_id);
                        handle_connection_closed(
                            peer_id,
                            num_established,
                            &routes_loop,
                            &known_peers_loop,
                            &peer_events_tx_loop,
                        );
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        if let Some(pid) = peer_id {
                            pending_dials.remove(&pid);
                            reconnect_targets.insert(pid);
                            let attempt = reconnect_attempts.entry(pid).or_insert(0);
                            *attempt = attempt.saturating_add(1);
                            reconnect_next_due.insert(
                                pid,
                                tokio::time::Instant::now() + reconnect_backoff_duration(*attempt),
                            );
                        }
                        log::warn!(
                            "Outgoing connection error (composite, peer={:?}): {}",
                            peer_id,
                            error
                        );
                    }
                    SwarmEvent::NewListenAddr { address, .. } => {
                        log::info!("ActorSwarm listening on {address} (composite)");
                    }
                    _ => {}
                }
                }
            }
        }
    });

    Ok(finalize_bootstrap(
        local_peer_id,
        ctx,
        listen_addr,
        MeshTransportMode::Lan,
        swarm_cmd_tx,
        config.stream_reconnect_grace,
    ))
}
