use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures_util::StreamExt as _;
use kameo::remote;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle};
use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use tokio::sync::{mpsc, oneshot};

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

fn messaging_config(request_timeout: std::time::Duration) -> remote::messaging::Config {
    remote::messaging::Config::default()
        .with_request_timeout(request_timeout)
        .with_request_size_maximum(crate::provider_transport::MESH_MESSAGE_SIZE_MAXIMUM)
        .with_response_size_maximum(crate::provider_transport::MESH_MESSAGE_SIZE_MAXIMUM)
}

async fn next_mesh_event<B: NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
) -> SwarmEvent<B::ToSwarm> {
    // Kameo 0.22 polls one command at a time but does not self-wake after
    // Unregister/LookupLocal. Queued commands can then sleep indefinitely on
    // idle meshes. Re-poll locally until upstream drains its command queue;
    // this watchdog sends no network traffic and never replays requests.
    let mut poll_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = poll_tick.tick() => {},
            event = swarm.select_next_some() => return event,
        }
    }
}

fn iroh_transport_config(config: &MeshRuntimeConfig) -> libp2p_iroh::TransportConfig {
    libp2p_iroh::TransportConfig {
        timeout: config.request_timeout,
        enable_gso: config.iroh_gso,
        ..Default::default()
    }
}

async fn finish_swarm_shutdown<F, T>(
    endpoint_close: Option<F>,
    teardown: T,
    completion: Option<oneshot::Sender<()>>,
) where
    F: std::future::Future<Output = ()>,
{
    if let Some(endpoint_close) = endpoint_close {
        endpoint_close.await;
    }
    drop(teardown);
    if let Some(completion) = completion {
        let _ = completion.send(());
    }
}

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

    let mut iroh_endpoint = None;
    let mut swarm: libp2p::Swarm<UnifiedMeshBehaviour> = if has_lan && has_iroh {
        let iroh_config = iroh_transport_config(config);
        let iroh_transport = libp2p_iroh::Transport::with_config(Some(&keypair), iroh_config)
            .await
            .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;
        iroh_endpoint = Some(iroh_transport.endpoint().clone());

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
                let kameo_behaviour =
                    remote::Behaviour::new(local_peer_id, messaging_config(config.request_timeout));
                let mdns_behaviour = if enable_mdns {
                    let mdns_config = libp2p::mdns::Config {
                        ttl: std::time::Duration::from_secs(30),
                        query_interval: std::time::Duration::from_secs(15),
                        ..libp2p::mdns::Config::default()
                    };
                    Some(libp2p::mdns::tokio::Behaviour::new(
                        mdns_config,
                        local_peer_id,
                    )?)
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
                let kameo_behaviour =
                    remote::Behaviour::new(local_peer_id, messaging_config(config.request_timeout));
                let mdns_behaviour = if enable_mdns {
                    let mdns_config = libp2p::mdns::Config {
                        ttl: std::time::Duration::from_secs(30),
                        query_interval: std::time::Duration::from_secs(15),
                        ..libp2p::mdns::Config::default()
                    };
                    Some(libp2p::mdns::tokio::Behaviour::new(
                        mdns_config,
                        local_peer_id,
                    )?)
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
        let iroh_config = iroh_transport_config(config);
        let iroh_transport = libp2p_iroh::Transport::with_config(Some(&keypair), iroh_config)
            .await
            .map_err(|e| MeshError::SwarmError(format!("iroh transport init failed: {e}")))?;
        iroh_endpoint = Some(iroh_transport.endpoint().clone());

        let local_peer_id = iroh_transport.peer_id;
        let behaviour = UnifiedMeshBehaviour {
            kameo: remote::Behaviour::new(local_peer_id, messaging_config(config.request_timeout)),
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
                        SwarmCommand::Shutdown { completion } => {
                            reconnect_targets.clear();
                            reconnect_targets_by_scope.clear();
                            pending_dials.clear();
                            reconnect_attempts.clear();
                            reconnect_next_due.clear();
                            peer_iroh_scope_loop.clear();
                            finish_swarm_shutdown(
                                iroh_endpoint.as_ref().map(|endpoint| endpoint.close()),
                                swarm,
                                completion,
                            ).await;
                            break;
                        }
                    }
                }
                event = next_mesh_event(&mut swarm) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::io::Cursor;
    use libp2p::request_response::Codec as _;
    use parking_lot::RwLock;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn test_runtime_handle(
        mode: MeshTransportMode,
    ) -> (MeshRuntimeHandle, mpsc::UnboundedReceiver<SwarmCommand>) {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = keypair.public().to_peer_id();
        let (peer_events_tx, _peer_events_rx) = tokio::sync::broadcast::channel(8);
        let routes = Arc::new(crate::mesh_routes::RouteTable::new(
            std::time::Duration::from_secs(60),
        ));
        let re_register_fns = Arc::new(RwLock::new(HashMap::new()));
        let (swarm_cmd_tx, swarm_cmd_rx) = mpsc::unbounded_channel();
        let mesh = MeshHandle::new(
            peer_id,
            peer_events_tx,
            routes,
            "test-host".to_string(),
            re_register_fns,
            keypair,
            None,
            None,
            mode,
            swarm_cmd_tx,
            std::time::Duration::from_secs(30),
        );
        (MeshRuntimeHandle::new(mesh), swarm_cmd_rx)
    }

    async fn local_endpoint() -> iroh::Endpoint {
        iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
            .secret_key(iroh::SecretKey::generate())
            .alpns(vec![b"/querymt/shutdown-test/1".to_vec()])
            .clear_ip_transports()
            .bind_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .portmapper_config(iroh::endpoint::PortmapperConfig::Disabled)
            .net_report_config(iroh::endpoint::NetReportConfig::minimal())
            .transport_config(
                iroh::endpoint::QuicTransportConfig::builder()
                    .enable_segmentation_offload(false)
                    .build(),
            )
            .bind()
            .await
            .unwrap()
    }

    #[test]
    fn iroh_gso_is_forwarded_for_iroh_only_and_mixed_transports() {
        for has_lan in [false, true] {
            for iroh_gso in [false, true] {
                let config = MeshRuntimeConfig {
                    enabled: true,
                    lan: has_lan.then_some(crate::LanMeshConfig {
                        listen: None,
                        discovery: LanDiscovery::None,
                        directory: crate::DirectoryMode::default(),
                    }),
                    iroh_enabled: true,
                    iroh_gso,
                    iroh_scopes: Vec::new(),
                    identity_file: None,
                    request_timeout: std::time::Duration::from_secs(19),
                    stream_reconnect_grace: std::time::Duration::from_secs(120),
                    node_name: None,
                    peers: Vec::new(),
                    auto_fallback: false,
                };
                let transport = iroh_transport_config(&config);
                assert_eq!(transport.enable_gso, iroh_gso);
                assert_eq!(transport.timeout, config.request_timeout);
                assert_eq!(
                    transport.relay_mode,
                    libp2p_iroh::TransportConfig::default().relay_mode
                );
                assert!(transport.peer_filter.is_none());
            }
        }
    }

    #[tokio::test]
    async fn shutdown_ack_waits_for_endpoint_close() {
        let (close_started_tx, close_started_rx) = oneshot::channel();
        let (release_close_tx, release_close_rx) = oneshot::channel();
        let (completion_tx, mut completion_rx) = oneshot::channel();
        let shutdown = tokio::spawn(async move {
            finish_swarm_shutdown(
                Some(async move {
                    close_started_tx.send(()).unwrap();
                    release_close_rx.await.unwrap();
                }),
                (),
                Some(completion_tx),
            )
            .await;
        });

        tokio::time::timeout(std::time::Duration::from_secs(2), close_started_rx)
            .await
            .expect("endpoint close future was not polled")
            .unwrap();
        assert!(matches!(
            completion_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        release_close_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), completion_rx)
            .await
            .expect("shutdown completion was not sent after endpoint close")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown task did not finish")
            .unwrap();
    }

    #[tokio::test]
    async fn shutdown_drops_owned_listener_before_ack() {
        struct TeardownProbe {
            completion_rx: Arc<parking_lot::Mutex<oneshot::Receiver<()>>>,
            dropped_before_ack: Arc<parking_lot::Mutex<bool>>,
            _listener: std::net::TcpListener,
        }

        impl Drop for TeardownProbe {
            fn drop(&mut self) {
                *self.dropped_before_ack.lock() = matches!(
                    self.completion_rx.lock().try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                );
            }
        }

        let listener =
            std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let listener_addr = listener.local_addr().unwrap();
        let (completion_tx, completion_rx) = oneshot::channel();
        let completion_rx = Arc::new(parking_lot::Mutex::new(completion_rx));
        let dropped_before_ack = Arc::new(parking_lot::Mutex::new(false));
        let teardown = TeardownProbe {
            completion_rx: completion_rx.clone(),
            dropped_before_ack: dropped_before_ack.clone(),
            _listener: listener,
        };

        finish_swarm_shutdown(
            None::<std::future::Ready<()>>,
            teardown,
            Some(completion_tx),
        )
        .await;

        assert!(
            *dropped_before_ack.lock(),
            "shutdown acknowledged before owned teardown was dropped"
        );
        assert!(matches!(completion_rx.lock().try_recv(), Ok(())));
        std::net::TcpListener::bind(listener_addr)
            .expect("owned listener was not released before shutdown acknowledgement");
    }

    #[tokio::test]
    async fn shutdown_waits_for_iroh_peer_close_and_serializes_repeats() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let (runtime, mut swarm_cmd_rx) = test_runtime_handle(MeshTransportMode::Iroh);
            let local = local_endpoint().await;
            let peer = local_endpoint().await;
            let addr = iroh::EndpointAddr::new(local.id()).with_ip_addr(local.bound_sockets()[0]);
            let (local_connection, peer_connection) = tokio::join!(
                async { local.accept().await.unwrap().await.unwrap() },
                peer.connect(addr, b"/querymt/shutdown-test/1"),
            );
            let peer_connection = peer_connection.unwrap();

            let (mut peer_send, _peer_recv) = peer_connection.open_bi().await.unwrap();
            peer_send.write_all(b"live").await.unwrap();
            peer_send.finish().unwrap();
            let (_local_send, mut local_recv) = local_connection.accept_bi().await.unwrap();
            let mut local_payload = [0; 4];
            local_recv.read_exact(&mut local_payload).await.unwrap();
            assert_eq!(&local_payload, b"live");

            let local_for_loop = local.clone();
            let driver = tokio::spawn(async move {
                match swarm_cmd_rx.recv().await.unwrap() {
                    SwarmCommand::Shutdown { completion } => {
                        finish_swarm_shutdown(Some(local_for_loop.close()), (), completion).await;
                    }
                    other => panic!("expected Shutdown command, got {other:?}"),
                }
            });
            tokio::join!(runtime.shutdown(), runtime.shutdown());
            driver.await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(3), peer_connection.closed())
                .await
                .expect("peer connection did not observe endpoint shutdown");
            assert!(matches!(
                peer_connection.close_reason(),
                Some(iroh::endpoint::ConnectionError::ApplicationClosed(close))
                    if close.error_code == 0_u32.into()
            ));
            peer.close().await;
        })
        .await
        .expect("graceful shutdown test timed out");
    }

    #[tokio::test]
    async fn shutdown_without_iroh_endpoint_completes() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (runtime, mut swarm_cmd_rx) = test_runtime_handle(MeshTransportMode::Lan);
            let driver = tokio::spawn(async move {
                match swarm_cmd_rx.recv().await.unwrap() {
                    SwarmCommand::Shutdown { completion } => {
                        finish_swarm_shutdown(None::<std::future::Ready<()>>, (), completion).await;
                    }
                    other => panic!("expected Shutdown command, got {other:?}"),
                }
            });

            runtime.shutdown().await;
            driver.await.unwrap();
        })
        .await
        .expect("LAN-only shutdown test timed out");
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct LargeRequest(Vec<u8>);

    #[derive(Debug, Serialize, Deserialize)]
    struct EmptyResponse;

    #[tokio::test]
    async fn idle_mesh_drains_queued_unregister_commands() {
        let peer_id = PeerId::random();
        let behaviour =
            remote::Behaviour::new(peer_id, messaging_config(std::time::Duration::from_secs(1)));
        behaviour.init_global();
        let transport = libp2p::core::transport::dummy::DummyTransport::<(
            PeerId,
            libp2p::core::muxing::StreamMuxerBox,
        )>::new();
        let mut swarm = libp2p::Swarm::new(
            libp2p::Transport::boxed(transport),
            behaviour,
            peer_id,
            libp2p::swarm::Config::with_tokio_executor(),
        );
        let driver = tokio::spawn(async move {
            loop {
                next_mesh_event(&mut swarm).await;
            }
        });
        // No network or discovery event should be needed to drain local commands.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            futures_util::future::join_all(
                (0..8).map(|index| remote::unregister(format!("absent-{index}"))),
            ),
        )
        .await;
        driver.abort();
        let replies = result.expect("queued unregister commands must not stall an idle mesh");
        assert!(replies.into_iter().all(|reply| reply.is_ok()));
    }

    #[tokio::test]
    async fn shared_messaging_config_applies_the_request_limit() {
        let protocol = libp2p::StreamProtocol::new("/querymt/mesh-config-test/1");
        let request = LargeRequest(vec![0_u8; 2 * 1024 * 1024]);
        let encoded = cbor4ii::serde::to_vec(Vec::new(), &request).unwrap();
        assert!(encoded.len() as u64 > 1024 * 1024);
        assert!(encoded.len() as u64 <= crate::provider_transport::MESH_MESSAGE_SIZE_MAXIMUM);

        let mut codec: libp2p::request_response::cbor::codec::Codec<LargeRequest, EmptyResponse> =
            messaging_config(std::time::Duration::from_secs(1)).into();
        let decoded = codec
            .read_request(&protocol, &mut Cursor::new(encoded))
            .await
            .expect("the common mesh config must raise the request limit");
        assert_eq!(decoded.0.len(), 2 * 1024 * 1024);
    }
}
