use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::StreamExt as _;
use kameo::remote;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle};
use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use tokio::sync::mpsc;

use crate::mesh_bootstrap::{MeshBootstrapContext, finalize_bootstrap, prepare_runtime_bootstrap};
use crate::mesh_events::{
    connection_route_plan, handle_connection_closed, handle_connection_established,
    handle_mdns_discovered, handle_mdns_expired, log_kameo_messaging_event, peer_id_from_multiaddr,
    reconnect_backoff_duration, refresh_mesh_state_known_peers, seed_scoped_dial_peer,
    should_dial_peer_command,
};
use crate::mesh_runtime_support::{DialReason, SwarmCommand, resolve_local_hostname};
use crate::{
    LanDiscovery, MeshError, MeshHandle, MeshRuntimeConfig, MeshRuntimeHandle, MeshScopeId,
    MeshStateStore, MeshTransportMode, SignedInviteGrant, default_mesh_state_path,
};

pub async fn bootstrap_mesh_runtime(
    config: &MeshRuntimeConfig,
) -> Result<MeshRuntimeHandle, MeshError> {
    let handle = bootstrap_mesh_handle(config).await?;
    Ok(MeshRuntimeHandle::new(handle))
}

pub async fn bootstrap_mesh_handle(config: &MeshRuntimeConfig) -> Result<MeshHandle, MeshError> {
    let has_lan = config.has_lan();
    let has_iroh = config.has_iroh();

    if !has_lan && !has_iroh {
        return Err(MeshError::SwarmError(
            "no transport enabled in MeshRuntimeConfig".to_string(),
        ));
    }

    let transport_mode = match (has_lan, has_iroh) {
        (true, false) => MeshTransportMode::Lan,
        (false, true) => MeshTransportMode::Iroh,
        (true, true) => MeshTransportMode::Composite,
        _ => unreachable!(),
    };

    let ctx = prepare_runtime_bootstrap(
        config.identity_file.as_deref(),
        &config.peers,
        resolve_local_hostname(),
    )?;

    let MeshBootstrapContext {
        keypair,
        peer_events_tx,
        routes,
        known_peers,
        re_register_fns,
        local_hostname,
    } = ctx;

    let peer_events_tx_loop = peer_events_tx.clone();
    let known_peers_loop = Arc::clone(&known_peers);
    let routes_loop = Arc::clone(&routes);
    let re_register_fns_loop = Arc::clone(&re_register_fns);

    let enable_mdns = has_lan
        && config
            .lan
            .as_ref()
            .is_some_and(|l| matches!(l.discovery, LanDiscovery::Mdns));

    let lan_listen_addr = config
        .lan
        .as_ref()
        .and_then(|l| l.listen.as_deref())
        .unwrap_or("/ip4/0.0.0.0/tcp/0");

    let iroh_invites: Vec<(SignedInviteGrant, String)> = if has_iroh {
        let mut invites = Vec::new();
        for scope in &config.iroh_scopes {
            if let Some(ref invite_str) = scope.invite {
                let grant = SignedInviteGrant::decode(invite_str).map_err(|e| {
                    MeshError::SwarmError(format!(
                        "invalid invite for scope '{}': {e}",
                        scope.mesh_id
                    ))
                })?;
                invites.push((grant, scope.mesh_id.clone()));
            }
        }
        invites
    } else {
        Vec::new()
    };

    let mesh_state_store_loop: Option<Arc<RwLock<MeshStateStore>>> = if has_iroh {
        default_mesh_state_path()
            .ok()
            .and_then(|p| MeshStateStore::load_or_create(&p).ok())
            .map(|s| Arc::new(RwLock::new(s)))
    } else {
        None
    };

    #[derive(NetworkBehaviour)]
    struct UnifiedMeshBehaviour {
        kameo: remote::Behaviour,
        mdns: Toggle<libp2p::mdns::tokio::Behaviour>,
    }

    let mut swarm: libp2p::Swarm<UnifiedMeshBehaviour> = if has_lan && has_iroh {
        let iroh_config = libp2p_iroh::TransportConfig {
            timeout: config.request_timeout,
            ..Default::default()
        };
        let iroh_transport = libp2p_iroh::Transport::with_config(Some(&keypair), iroh_config)
            .await
            .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

        libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )
            .map_err(|e| MeshError::SwarmError(e.to_string()))?
            .with_quic()
            .with_other_transport(move |_| iroh_transport)
            .map_err(|e: std::convert::Infallible| -> MeshError { match e {} })?
            .with_behaviour(|key| {
                let local_peer_id = key.public().to_peer_id();
                let kameo_behaviour = remote::Behaviour::new(
                    local_peer_id,
                    remote::messaging::Config::default()
                        .with_request_timeout(config.request_timeout)
                        .with_response_size_maximum(50 * 1024 * 1024),
                );
                let mdns_behaviour = if enable_mdns {
                    let mdns_config = libp2p::mdns::Config {
                        ttl: std::time::Duration::from_secs(30),
                        query_interval: std::time::Duration::from_secs(15),
                        ..libp2p::mdns::Config::default()
                    };
                    Some(libp2p::mdns::tokio::Behaviour::new(mdns_config, local_peer_id)?)
                } else {
                    None
                };
                Ok(UnifiedMeshBehaviour {
                    kameo: kameo_behaviour,
                    mdns: mdns_behaviour.into(),
                })
            })
            .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
            .with_swarm_config(|c| {
                c.with_idle_connection_timeout(std::time::Duration::from_secs(300))
            })
            .build()
    } else if has_lan {
        libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
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
                let mdns_behaviour = if enable_mdns {
                    let mdns_config = libp2p::mdns::Config {
                        ttl: std::time::Duration::from_secs(30),
                        query_interval: std::time::Duration::from_secs(15),
                        ..libp2p::mdns::Config::default()
                    };
                    Some(libp2p::mdns::tokio::Behaviour::new(mdns_config, local_peer_id)?)
                } else {
                    None
                };
                Ok(UnifiedMeshBehaviour {
                    kameo: kameo_behaviour,
                    mdns: mdns_behaviour.into(),
                })
            })
            .map_err(|e: libp2p::BehaviourBuilderError| MeshError::SwarmError(e.to_string()))?
            .with_swarm_config(|c| {
                c.with_idle_connection_timeout(std::time::Duration::from_secs(300))
            })
            .build()
    } else {
        let iroh_config = libp2p_iroh::TransportConfig {
            timeout: config.request_timeout,
            ..Default::default()
        };
        let iroh_transport = libp2p_iroh::Transport::with_config(Some(&keypair), iroh_config)
            .await
            .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;

        let local_peer_id = iroh_transport.peer_id;
        let behaviour = UnifiedMeshBehaviour {
            kameo: remote::Behaviour::new(
                local_peer_id,
                remote::messaging::Config::default()
                    .with_request_timeout(config.request_timeout)
                    .with_response_size_maximum(50 * 1024 * 1024),
            ),
            mdns: None.into(),
        };

        libp2p::Swarm::new(
            libp2p::Transport::boxed(iroh_transport),
            behaviour,
            local_peer_id,
            libp2p::swarm::Config::with_executor(Box::new(|fut| {
                tokio::spawn(fut);
            }))
            .with_idle_connection_timeout(std::time::Duration::from_secs(300)),
        )
    };

    swarm
        .behaviour()
        .kameo
        .try_init_global()
        .map_err(|e| MeshError::SwarmError(e.to_string()))?;

    let local_peer_id = *swarm.local_peer_id();

    if has_lan {
        swarm
            .listen_on(
                lan_listen_addr
                    .parse()
                    .map_err(|e: libp2p::multiaddr::Error| MeshError::InvalidListenAddr {
                        addr: lan_listen_addr.to_string(),
                        reason: e.to_string(),
                    })?,
            )
            .map_err(|e| MeshError::SwarmError(e.to_string()))?;
    }

    if has_iroh {
        swarm
            .listen_on(Multiaddr::empty())
            .map_err(|e| MeshError::SwarmError(e.to_string()))?;
    }

    for peer_addr in &config.peers {
        let addr: Multiaddr = peer_addr.parse().expect("validated above");
        match swarm.dial(addr.clone()) {
            Ok(_) => log::info!("Dialing bootstrap peer: {}", addr),
            Err(e) => log::warn!("Failed to dial bootstrap peer {}: {}", addr, e),
        }
    }

    for (invite, mesh_id) in &iroh_invites {
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
                "Dialing inviter via iroh relay: {} (mesh: {})",
                inviter_addr,
                mesh_id,
            ),
            Err(e) => log::warn!("Failed to dial inviter {}: {}", inviter_addr, e),
        }
    }

    let mut reconnect_targets: HashSet<PeerId> = HashSet::new();
    let mut reconnect_targets_by_scope: HashMap<String, HashSet<PeerId>> = HashMap::new();

    for (invite, mesh_id) in &iroh_invites {
        if let Ok(inviter_pid) = invite.grant.inviter_peer_id.parse::<PeerId>() {
            reconnect_targets.insert(inviter_pid);
            reconnect_targets_by_scope
                .entry(mesh_id.clone())
                .or_default()
                .insert(inviter_pid);
        }
    }

    for peer_addr in &config.peers {
        if let Ok(addr) = peer_addr.parse::<Multiaddr>()
            && let Some(peer_id) = peer_id_from_multiaddr(&addr)
        {
            reconnect_targets.insert(peer_id);
        }
    }

    if let Some(ref ms) = mesh_state_store_loop {
        let store = ms.read();
        for mesh_id in store.active_mesh_ids() {
            for peer in store.reconnect_peers_for_mesh(&mesh_id) {
                if let Ok(pid) = peer.peer_id.parse::<PeerId>() {
                    reconnect_targets.insert(pid);
                    reconnect_targets_by_scope
                        .entry(mesh_id.clone())
                        .or_default()
                        .insert(pid);
                }
            }
        }
    }

    reconnect_targets.remove(&local_peer_id);

    let (swarm_cmd_tx, mut swarm_cmd_rx) = mpsc::unbounded_channel::<SwarmCommand>();
    let has_lan_loop = has_lan;
    let has_iroh_loop = has_iroh;

    tokio::spawn(async move {
        let mut pending_dials: HashSet<PeerId> = HashSet::new();
        let mut reconnect_attempts: HashMap<PeerId, u32> = HashMap::new();
        let mut reconnect_next_due: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
        let mut peer_iroh_scope_loop: HashMap<PeerId, MeshScopeId> = reconnect_targets_by_scope
            .iter()
            .flat_map(|(mesh_id, pids)| {
                pids.iter().map(move |pid| {
                    (
                        *pid,
                        MeshScopeId::Iroh {
                            mesh_id: mesh_id.clone(),
                        },
                    )
                })
            })
            .collect();
        let mut reconnect_tick = tokio::time::interval(std::time::Duration::from_secs(5));
        reconnect_tick.tick().await;

        loop {
            tokio::select! {
                _ = reconnect_tick.tick(), if has_iroh_loop => {
                    let now = tokio::time::Instant::now();
                    for peer_id in reconnect_targets.iter().copied().collect::<Vec<_>>() {
                        if peer_id == local_peer_id {
                            continue;
                        }
                        if !should_dial_peer_command(&peer_id, DialReason::Reconnect, &peer_iroh_scope_loop, has_iroh_loop)
                            || routes_loop.is_peer_alive(&peer_id)
                            || pending_dials.contains(&peer_id)
                            || reconnect_next_due.get(&peer_id).is_some_and(|due| *due > now)
                        {
                            continue;
                        }

                        let addr: Multiaddr = format!("/p2p/{peer_id}").parse().expect("valid /p2p addr");
                        match swarm.dial(addr) {
                            Ok(_) => { pending_dials.insert(peer_id); }
                            Err(e) => {
                                let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                *attempt = attempt.saturating_add(1);
                                let delay = reconnect_backoff_duration(*attempt);
                                reconnect_next_due.insert(peer_id, now + delay);
                                log::warn!("Reconnect dial failed (unified, peer={}, attempt={}): {}", peer_id, *attempt, e);
                            }
                        }
                    }
                }
                Some(cmd) = swarm_cmd_rx.recv() => {
                    match cmd {
                        SwarmCommand::DialPeer { peer_id, scope, reason } => {
                            if !has_iroh_loop {
                                continue;
                            }
                            seed_scoped_dial_peer(peer_id, scope, &mut reconnect_targets_by_scope, &mut peer_iroh_scope_loop);
                            if !should_dial_peer_command(&peer_id, reason, &peer_iroh_scope_loop, has_iroh_loop) {
                                continue;
                            }
                            reconnect_targets.insert(peer_id);
                            if pending_dials.contains(&peer_id) || routes_loop.is_peer_alive(&peer_id) {
                                continue;
                            }
                            let addr: Multiaddr = format!("/p2p/{peer_id}").parse().expect("valid /p2p addr");
                            match swarm.dial(addr) {
                                Ok(_) => { pending_dials.insert(peer_id); }
                                Err(e) => {
                                    let attempt = reconnect_attempts.entry(peer_id).or_insert(0);
                                    *attempt = attempt.saturating_add(1);
                                    reconnect_next_due.insert(peer_id, tokio::time::Instant::now() + reconnect_backoff_duration(*attempt));
                                    log::warn!("Failed to dial peer {} (unified): {}", peer_id, e);
                                }
                            }
                        }
                        SwarmCommand::JoinIrohScope { mesh_id, peers } => {
                            let scope = MeshScopeId::Iroh { mesh_id: mesh_id.clone() };
                            let scoped_peers = reconnect_targets_by_scope.entry(mesh_id).or_default();
                            for peer_id in peers {
                                if peer_id == local_peer_id {
                                    continue;
                                }
                                reconnect_targets.insert(peer_id);
                                scoped_peers.insert(peer_id);
                                peer_iroh_scope_loop.insert(peer_id, scope.clone());
                                reconnect_next_due.remove(&peer_id);
                            }
                        }
                        SwarmCommand::LeaveIrohScope { mesh_id } => {
                            if let Some(peers) = reconnect_targets_by_scope.remove(&mesh_id) {
                                for pid in peers {
                                    reconnect_targets.remove(&pid);
                                    pending_dials.remove(&pid);
                                    reconnect_attempts.remove(&pid);
                                    reconnect_next_due.remove(&pid);
                                    peer_iroh_scope_loop.remove(&pid);
                                }
                            }
                        }
                    }
                }
                event = swarm.select_next_some() => {
                    match event {
                        SwarmEvent::Behaviour(UnifiedMeshBehaviourEvent::Kameo(remote::Event::Messaging(event))) => {
                            log_kameo_messaging_event(&event);
                        }
                        SwarmEvent::Behaviour(UnifiedMeshBehaviourEvent::Mdns(libp2p::mdns::Event::Discovered(list))) => {
                            handle_mdns_discovered(&mut swarm, list, &known_peers_loop, &routes_loop, &peer_events_tx_loop, &re_register_fns_loop);
                        }
                        SwarmEvent::Behaviour(UnifiedMeshBehaviourEvent::Mdns(libp2p::mdns::Event::Expired(list))) => {
                            handle_mdns_expired(&mut swarm, list, &known_peers_loop, &routes_loop, &peer_events_tx_loop);
                        }
                        SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                            pending_dials.remove(&peer_id);
                            reconnect_targets.insert(peer_id);
                            reconnect_attempts.remove(&peer_id);
                            reconnect_next_due.remove(&peer_id);
                            let remote_addr = endpoint.get_remote_address().clone();
                            let plan = connection_route_plan(has_lan_loop, has_iroh_loop, peer_iroh_scope_loop.get(&peer_id));
                            for (transport, scope, priority) in plan {
                                handle_connection_established(&mut swarm, peer_id, remote_addr.clone(), &routes_loop, &known_peers_loop, &peer_events_tx_loop, &re_register_fns_loop, transport, scope, priority);
                            }
                            refresh_mesh_state_known_peers(&mesh_state_store_loop, &routes_loop);
                        }
                        SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                            reconnect_targets.insert(peer_id);
                            reconnect_next_due.remove(&peer_id);
                            handle_connection_closed(peer_id, num_established, &routes_loop, &known_peers_loop, &peer_events_tx_loop);
                        }
                        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                            if let Some(pid) = peer_id {
                                pending_dials.remove(&pid);
                                reconnect_targets.insert(pid);
                                let attempt = reconnect_attempts.entry(pid).or_insert(0);
                                *attempt = attempt.saturating_add(1);
                                reconnect_next_due.insert(pid, tokio::time::Instant::now() + reconnect_backoff_duration(*attempt));
                            }
                            log::warn!("Outgoing connection error (unified, peer={:?}): {}", peer_id, error);
                        }
                        SwarmEvent::NewListenAddr { address, .. } => {
                            log::info!("ActorSwarm listening on {address}");
                        }
                        _ => {}
                    }
                }
            }
        }
    });

    let ctx = MeshBootstrapContext {
        keypair,
        peer_events_tx,
        routes,
        known_peers,
        re_register_fns,
        local_hostname,
    };

    let listen_label = match transport_mode {
        MeshTransportMode::Lan => lan_listen_addr.to_string(),
        MeshTransportMode::Iroh => "iroh-relay".to_string(),
        MeshTransportMode::Composite => format!("{}+iroh", lan_listen_addr),
    };

    let mut handle = finalize_bootstrap(
        local_peer_id,
        ctx,
        &listen_label,
        transport_mode,
        swarm_cmd_tx,
        config.stream_reconnect_grace,
    );
    handle.set_config_scopes(config.active_scopes());
    Ok(handle)
}
