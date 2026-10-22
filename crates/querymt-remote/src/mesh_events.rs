use crate::{MeshRoute, MeshScopeId, MeshStateStore, MeshTransportKind, PeerEntry};
use kameo::remote;
use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

use crate::mesh_handle::ReRegisterFn;
use crate::mesh_routes::RouteTable;
use crate::mesh_runtime_support::DialReason;

#[derive(Debug, Clone)]
pub enum MeshEvent {
    Discovered(PeerId),
    Expired(PeerId),
    RouteAdded { peer_id: PeerId, route: MeshRoute },
    RouteRemoved { peer_id: PeerId, route: MeshRoute },
    ScopeJoined(MeshScopeId),
    ScopeLeft(MeshScopeId),
}

pub type PeerEvent = MeshEvent;

pub(crate) fn handle_mdns_discovered<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    list: Vec<(PeerId, Multiaddr)>,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    routes: &RouteTable,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) {
    let mut addrs_by_peer: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    for (peer_id, multiaddr) in list {
        swarm.add_peer_address(peer_id, multiaddr.clone());
        addrs_by_peer.entry(peer_id).or_default().push(multiaddr);
    }

    for (peer_id, new_addrs) in addrs_by_peer {
        let (is_new, has_new_addr) = {
            let peers = known_peers.read();
            match peers.get(&peer_id) {
                None => (true, false),
                Some(known_addrs) => {
                    let any_new = new_addrs.iter().any(|a| !known_addrs.contains(a));
                    (false, any_new)
                }
            }
        };

        {
            let mut peers = known_peers.write();
            let entry = peers.entry(peer_id).or_default();
            for addr in &new_addrs {
                entry.insert(addr.clone());
            }
        }

        if is_new {
            log::info!("mDNS discovered peer: {peer_id}");
        } else if has_new_addr {
            log::info!(
                "mDNS re-discovered peer {peer_id} with new address(es): {:?}",
                new_addrs
            );
        } else {
            log::debug!("mDNS re-announced peer {peer_id} (refreshing route TTL)");
        }

        let was_alive = routes.is_peer_alive(&peer_id);
        let route = routes.upsert_addrs(
            peer_id,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            new_addrs.clone(),
            100,
        );
        let _ = peer_events_tx.send(PeerEvent::RouteAdded { peer_id, route });
        // A new address can restore access to a previously missed catalog even
        // when another connection already pins this peer's route.
        if !was_alive || has_new_addr {
            let _ = peer_events_tx.send(PeerEvent::Discovered(peer_id));
        }
        // Establish transport before publishing actors; discovery alone does not
        // guarantee that a DHT publication will initiate a connection.
        if !swarm.is_connected(&peer_id) {
            let opts = libp2p::swarm::dial_opts::DialOpts::peer_id(peer_id)
                .condition(libp2p::swarm::dial_opts::PeerCondition::DisconnectedAndNotDialing)
                .addresses(new_addrs)
                .build();
            if let Err(error) = swarm.dial(opts) {
                log::debug!("Failed to dial discovered peer {peer_id}: {error}");
            }
        }
    }
}

pub(crate) fn handle_mdns_expired<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    list: Vec<(PeerId, Multiaddr)>,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    routes: &RouteTable,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) {
    let mut addrs_by_peer: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    for (peer_id, multiaddr) in list {
        addrs_by_peer.entry(peer_id).or_default().push(multiaddr);
    }

    for (peer_id, expired_addrs) in addrs_by_peer {
        let peer_fully_gone = {
            let mut peers = known_peers.write();
            if let Some(known_addrs) = peers.get_mut(&peer_id) {
                for addr in &expired_addrs {
                    known_addrs.remove(addr);
                }
                if known_addrs.is_empty() {
                    peers.remove(&peer_id);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        let expired_set: HashSet<Multiaddr> = expired_addrs.into_iter().collect();
        let removed = routes.routes_for_peer(&peer_id).into_iter().find(|route| {
            route.transport == MeshTransportKind::Lan && route.scope == MeshScopeId::lan_default()
        });
        routes.remove_addrs(
            peer_id,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            &expired_set,
        );
        if let Some(mut route) = removed {
            route.addrs.retain(|addr| expired_set.contains(addr));
            let _ = peer_events_tx.send(PeerEvent::RouteRemoved { peer_id, route });
        }

        // Discovery expiry is not a transport failure. Keep established connections,
        // including LAN connections whose mDNS responses were temporarily lost.
        if peer_fully_gone && !swarm.is_connected(&peer_id) && !routes.is_peer_alive(&peer_id) {
            log::info!("mDNS peer expired (went away): {peer_id}");
            let _ = peer_events_tx.send(PeerEvent::Expired(peer_id));
        }
    }
}

pub(crate) fn connection_transport(remote_addr: &Multiaddr) -> MeshTransportKind {
    if remote_addr
        .iter()
        .all(|protocol| matches!(protocol, libp2p::multiaddr::Protocol::P2p(_)))
    {
        MeshTransportKind::Iroh
    } else {
        MeshTransportKind::Lan
    }
}

pub(crate) fn connection_route_plan(
    has_lan: bool,
    has_iroh: bool,
    iroh_scope: Option<&MeshScopeId>,
) -> Vec<(MeshTransportKind, MeshScopeId, u32)> {
    let mut plan = Vec::new();
    if has_lan {
        plan.push((MeshTransportKind::Lan, MeshScopeId::lan_default(), 100));
    }
    if has_iroh && let Some(scope) = iroh_scope {
        plan.push((MeshTransportKind::Iroh, scope.clone(), 70));
    }
    plan
}

pub(crate) fn refresh_mesh_state_known_peers(
    mesh_state_store: &Option<Arc<RwLock<MeshStateStore>>>,
    routes: &RouteTable,
) {
    let Some(ms) = mesh_state_store.as_ref() else {
        return;
    };
    let mut peers_by_scope: HashMap<String, Vec<PeerEntry>> = HashMap::new();
    for peer_id in routes.peer_ids() {
        for route in routes.routes_for_peer(&peer_id) {
            if route.transport == MeshTransportKind::Iroh
                && let MeshScopeId::Iroh { mesh_id } = route.scope
            {
                peers_by_scope.entry(mesh_id).or_default().push(PeerEntry {
                    peer_id: peer_id.to_string(),
                    addrs: vec![format!("/p2p/{peer_id}")],
                });
            }
        }
    }
    let ms = Arc::clone(ms);
    tokio::task::spawn_blocking(move || {
        let mut store = ms.write();
        for (mesh_id, peers) in peers_by_scope {
            // Keep offline members as reconnect targets, without importing LAN peers
            // or peers from other scopes into this mesh.
            let mut merged: HashMap<_, _> = store
                .known_peers_for_mesh(&mesh_id)
                .into_iter()
                .map(|peer| (peer.peer_id.clone(), peer))
                .collect();
            merged.extend(peers.into_iter().map(|peer| (peer.peer_id.clone(), peer)));
            if let Err(error) = store.update_known_peers(&mesh_id, merged.into_values().collect()) {
                log::warn!("Failed to persist peers for mesh {mesh_id}: {error}");
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_connection_established<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    peer_id: PeerId,
    remote_addr: Multiaddr,
    routes: &RouteTable,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
    transport: MeshTransportKind,
    scope: MeshScopeId,
    priority: u32,
) {
    swarm.add_peer_address(peer_id, remote_addr.clone());

    let was_alive = routes.is_peer_alive(&peer_id);
    let route =
        routes.connection_established(peer_id, transport, scope, remote_addr.clone(), priority);
    let _ = peer_events_tx.send(PeerEvent::RouteAdded { peer_id, route });

    let is_new = {
        let mut peers = known_peers.write();
        let entry = peers.entry(peer_id).or_default();
        let was_empty = entry.is_empty();
        entry.insert(remote_addr.clone());
        was_empty
    };

    if !was_alive || is_new {
        log::info!("Connected to peer: {peer_id} at {remote_addr}");
        let _ = peer_events_tx.send(PeerEvent::Discovered(peer_id));
    }
}

pub(crate) type MeshConnections =
    HashMap<libp2p::swarm::ConnectionId, (PeerId, MeshTransportKind, Multiaddr)>;

pub(crate) fn attach_iroh_scope_routes<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    scope: &MeshScopeId,
    peers: &[PeerId],
    connections: &MeshConnections,
    routes: &RouteTable,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) -> bool {
    let mut changed = false;
    for peer_id in peers {
        if routes
            .routes_for_peer(peer_id)
            .iter()
            .any(|route| &route.scope == scope)
        {
            continue;
        }
        if let Some((_, _, addr)) = connections
            .values()
            .find(|(peer, transport, _)| peer == peer_id && *transport == MeshTransportKind::Iroh)
        {
            handle_connection_established(
                swarm,
                *peer_id,
                addr.clone(),
                routes,
                known_peers,
                peer_events_tx,
                MeshTransportKind::Iroh,
                scope.clone(),
                70,
            );
            changed = true;
        }
    }
    changed
}

pub(crate) async fn run_re_registration_task(
    mut rx: mpsc::Receiver<()>,
    re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
) {
    while rx.recv().await.is_some() {
        // One publication pass per connection burst, never concurrent passes.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        while rx.try_recv().is_ok() {}
        let fns: Vec<_> = re_register_fns.read().values().cloned().collect();
        for register in fns {
            register().await;
        }
    }
}

pub(crate) fn handle_connection_closed(
    peer_id: PeerId,
    num_established: u32,
    routes: &RouteTable,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) {
    if num_established == 0 {
        let removed_routes = routes.routes_for_peer(&peer_id);
        let had_routes = !removed_routes.is_empty();
        routes.remove_peer(&peer_id);
        for route in removed_routes {
            let _ = peer_events_tx.send(PeerEvent::RouteRemoved { peer_id, route });
        }

        let was_known = {
            let mut peers = known_peers.write();
            peers.remove(&peer_id).is_some()
        };
        if was_known || had_routes {
            log::info!("Peer disconnected (no remaining connections): {peer_id}");
            let _ = peer_events_tx.send(PeerEvent::Expired(peer_id));
        }
    }
}

pub(crate) fn peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    use libp2p::multiaddr::Protocol;

    addr.iter().find_map(|p| match p {
        Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}

pub(crate) fn reconnect_backoff_duration(attempt: u32) -> std::time::Duration {
    let secs = (1u64 << attempt.min(9)).min(300);
    std::time::Duration::from_secs(secs)
}

pub(crate) fn should_dial_peer_command(
    peer_id: &PeerId,
    reason: DialReason,
    peer_iroh_scope: &HashMap<PeerId, MeshScopeId>,
    has_iroh: bool,
) -> bool {
    if !has_iroh {
        return false;
    }
    match reason {
        DialReason::Admission | DialReason::ExistingMeshPeer => true,
        DialReason::Reconnect | DialReason::Manual => peer_iroh_scope.contains_key(peer_id),
    }
}

pub(crate) fn seed_scoped_dial_peer(
    peer_id: PeerId,
    scope: Option<MeshScopeId>,
    reconnect_targets_by_scope: &mut HashMap<String, HashSet<PeerId>>,
    peer_iroh_scope: &mut HashMap<PeerId, MeshScopeId>,
) {
    if let Some(MeshScopeId::Iroh { mesh_id }) = scope.clone() {
        reconnect_targets_by_scope
            .entry(mesh_id)
            .or_default()
            .insert(peer_id);
        peer_iroh_scope.insert(peer_id, scope.unwrap());
    }
}

pub(crate) fn log_kameo_messaging_event(event: &remote::messaging::Event) {
    match event {
        remote::messaging::Event::AskResult {
            peer,
            connection_id,
            request_id,
            result,
        } => match result {
            Ok(_) => {
                tracing::debug!(target: "remote::mesh::messaging", peer = %peer, connection_id = ?connection_id, request_id = %request_id, "kameo ask completed")
            }
            Err(error) => {
                tracing::warn!(target: "remote::mesh::messaging", peer = %peer, connection_id = ?connection_id, request_id = %request_id, error = ?error, "kameo ask failed")
            }
        },
        remote::messaging::Event::TellResult {
            peer,
            connection_id,
            request_id,
            result,
        } => match result {
            Ok(()) => {
                tracing::debug!(target: "remote::mesh::messaging", peer = %peer, connection_id = ?connection_id, request_id = %request_id, "kameo tell acknowledged")
            }
            Err(error) => {
                tracing::warn!(target: "remote::mesh::messaging", peer = %peer, connection_id = ?connection_id, request_id = %request_id, error = %error, "kameo tell failed")
            }
        },
        remote::messaging::Event::LinkResult {
            peer,
            connection_id,
            request_id,
            result,
        } => {
            tracing::debug!(target: "remote::mesh::messaging", peer = %peer, connection_id = ?connection_id, request_id = %request_id, ok = result.is_ok(), "kameo link result")
        }
        remote::messaging::Event::UnlinkResult {
            peer,
            connection_id,
            request_id,
            result,
        } => {
            tracing::debug!(target: "remote::mesh::messaging", peer = %peer, connection_id = ?connection_id, request_id = %request_id, ok = result.is_ok(), "kameo unlink result")
        }
        remote::messaging::Event::SignalLinkDiedResult {
            peer,
            connection_id,
            request_id,
            result,
        } => {
            tracing::debug!(target: "remote::mesh::messaging", peer = %peer, connection_id = ?connection_id, request_id = %request_id, ok = result.is_ok(), "kameo signal_link_died result")
        }
        remote::messaging::Event::OutboundFailure {
            peer,
            connection_id,
            request_id,
            error,
        } => {
            tracing::warn!(target: "remote::mesh::messaging", peer = %peer, connection_id = %connection_id, request_id = %request_id, error = %error, "kameo outbound failure")
        }
        remote::messaging::Event::InboundFailure {
            peer,
            connection_id,
            request_id,
            error,
        } => {
            tracing::warn!(target: "remote::mesh::messaging", peer = %peer, connection_id = %connection_id, request_id = %request_id, error = ?error, "kameo inbound failure")
        }
        remote::messaging::Event::ResponseSent {
            peer,
            connection_id,
            request_id,
        } => {
            tracing::trace!(target: "remote::mesh::messaging", peer = %peer, connection_id = %connection_id, request_id = %request_id, "kameo response sent")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_swarm() -> libp2p::Swarm<libp2p::swarm::dummy::Behaviour> {
        libp2p::SwarmBuilder::with_new_identity()
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )
            .unwrap()
            .with_behaviour(|_| libp2p::swarm::dummy::Behaviour)
            .unwrap()
            .build()
    }

    #[tokio::test]
    async fn discovery_connects_before_actor_registration() {
        use futures_util::StreamExt;
        let mut local = test_swarm();
        let mut remote = test_swarm();
        let peer = *remote.local_peer_id();
        remote
            .listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap())
            .unwrap();
        let address = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } =
                    remote.select_next_some().await
                {
                    break address;
                }
            }
        })
        .await
        .unwrap();
        let known = RwLock::new(HashMap::new());
        let routes = RouteTable::new(Duration::from_secs(90));
        let (tx, _rx) = broadcast::channel(16);
        handle_mdns_discovered(&mut local, vec![(peer, address)], &known, &routes, &tx);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    event = local.select_next_some() => {
                        if let libp2p::swarm::SwarmEvent::ConnectionEstablished { peer_id, .. } = event {
                            assert_eq!(peer_id, peer);
                            break;
                        }
                    }
                    _ = remote.select_next_some() => {}
                }
            }
        }).await.unwrap();
        assert!(local.is_connected(&peer));
    }

    #[tokio::test]
    async fn duplicate_discovery_does_not_emit_peer_churn() {
        let mut swarm = test_swarm();
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let address: Multiaddr = format!("/ip4/127.0.0.1/tcp/1/p2p/{peer}").parse().unwrap();
        let known = RwLock::new(HashMap::new());
        let routes = RouteTable::new(Duration::from_secs(90));
        let (tx, mut rx) = broadcast::channel(16);
        for _ in 0..3 {
            handle_mdns_discovered(
                &mut swarm,
                vec![(peer, address.clone())],
                &known,
                &routes,
                &tx,
            );
        }
        let mut discovered = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, MeshEvent::Discovered(_)) {
                discovered += 1;
            }
        }
        assert_eq!(discovered, 1);

        // A genuinely new address is a recovery opportunity even while the
        // existing connection keeps the peer's route alive.
        routes.connection_established(
            peer,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            address,
            100,
        );
        let new_address: Multiaddr = format!("/ip4/127.0.0.1/tcp/2/p2p/{peer}").parse().unwrap();
        for _ in 0..3 {
            handle_mdns_discovered(
                &mut swarm,
                vec![(peer, new_address.clone())],
                &known,
                &routes,
                &tx,
            );
        }
        let notifications = std::iter::from_fn(|| rx.try_recv().ok())
            .filter(|event| matches!(event, MeshEvent::Discovered(id) if *id == peer))
            .count();
        assert_eq!(
            notifications, 1,
            "new addresses must trigger one inventory retry, not one per announcement"
        );
    }

    #[tokio::test]
    async fn mdns_expiry_preserves_connection_and_other_transport() {
        let mut swarm = test_swarm();
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let address: Multiaddr = format!("/ip4/127.0.0.1/tcp/1/p2p/{peer}").parse().unwrap();
        let known = RwLock::new(HashMap::from([(peer, HashSet::from([address.clone()]))]));
        let routes = RouteTable::new(Duration::from_secs(90));
        routes.connection_established(
            peer,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            address.clone(),
            100,
        );
        routes.connection_established(
            peer,
            MeshTransportKind::Iroh,
            MeshScopeId::Iroh {
                mesh_id: "a".into(),
            },
            format!("/p2p/{peer}").parse().unwrap(),
            70,
        );
        let (tx, mut rx) = broadcast::channel(16);
        handle_mdns_expired(&mut swarm, vec![(peer, address)], &known, &routes, &tx);
        assert!(routes.is_peer_alive(&peer));
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event, MeshEvent::Expired(_)));
        }
        handle_connection_closed(peer, 1, &routes, &known, &tx);
        assert!(routes.is_peer_alive(&peer));
        handle_connection_closed(peer, 0, &routes, &known, &tx);
        assert!(!routes.is_peer_alive(&peer));
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .any(|event| matches!(event, MeshEvent::Expired(_)))
        );
    }

    #[tokio::test]
    async fn persisted_peers_are_isolated_by_iroh_scope_and_keep_offline_members() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = MeshStateStore::load_or_create(&dir.path().join("mesh.json")).unwrap();
        for id in ["a", "b"] {
            store.upsert_hosted_mesh(id.into(), None, None).unwrap();
        }
        store
            .update_known_peers(
                "a",
                vec![PeerEntry {
                    peer_id: "offline".into(),
                    addrs: vec![],
                }],
            )
            .unwrap();
        let store = Arc::new(RwLock::new(store));
        let routes = RouteTable::new(Duration::from_secs(90));
        let mut peers = Vec::new();
        for scope in [
            MeshScopeId::lan_default(),
            MeshScopeId::Iroh {
                mesh_id: "a".into(),
            },
            MeshScopeId::Iroh {
                mesh_id: "b".into(),
            },
        ] {
            let peer = libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id();
            let transport = if scope.is_iroh() {
                MeshTransportKind::Iroh
            } else {
                MeshTransportKind::Lan
            };
            routes.connection_established(
                peer,
                transport,
                scope,
                format!("/p2p/{peer}").parse().unwrap(),
                70,
            );
            peers.push(peer.to_string());
        }
        refresh_mesh_state_known_peers(&Some(store.clone()), &routes);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if store.read().known_peers_for_mesh("b").len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let a: HashSet<_> = store
            .read()
            .known_peers_for_mesh("a")
            .into_iter()
            .map(|peer| peer.peer_id)
            .collect();
        let b: HashSet<_> = store
            .read()
            .known_peers_for_mesh("b")
            .into_iter()
            .map(|peer| peer.peer_id)
            .collect();
        assert_eq!(a, HashSet::from(["offline".into(), peers[1].clone()]));
        assert_eq!(b, HashSet::from([peers[2].clone()]));
        assert!(!a.contains(&peers[0]));
    }

    #[tokio::test]
    async fn authorized_scope_attaches_unknown_inbound_iroh_connection_only_to_that_scope() {
        let mut swarm = test_swarm();
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let other_peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let scope = MeshScopeId::Iroh {
            mesh_id: "authorized".into(),
        };
        let connections = MeshConnections::from([
            (
                libp2p::swarm::ConnectionId::new_unchecked(1),
                (
                    peer,
                    MeshTransportKind::Iroh,
                    format!("/p2p/{peer}").parse().unwrap(),
                ),
            ),
            (
                libp2p::swarm::ConnectionId::new_unchecked(2),
                (
                    other_peer,
                    MeshTransportKind::Iroh,
                    format!("/p2p/{other_peer}").parse().unwrap(),
                ),
            ),
        ]);
        let routes = RouteTable::new(Duration::from_secs(90));
        let known = RwLock::new(HashMap::new());
        let (tx, mut rx) = broadcast::channel(16);
        assert!(
            !routes.is_peer_alive(&peer),
            "transport connection alone must not grant scope membership"
        );
        attach_iroh_scope_routes(
            &mut swarm,
            &scope,
            &[peer],
            &connections,
            &routes,
            &known,
            &tx,
        );
        let route = routes.best_route_for_peer(&peer).unwrap();
        assert_eq!(route.transport, MeshTransportKind::Iroh);
        assert_eq!(route.scope, scope);
        assert!(!routes.is_peer_alive(&other_peer));
        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .any(|event| matches!(event, MeshEvent::Discovered(id) if id == peer))
        );
        // Readmission is idempotent and must not create another discovery storm.
        attach_iroh_scope_routes(
            &mut swarm,
            &scope,
            &[peer],
            &connections,
            &routes,
            &known,
            &tx,
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn registration_bursts_are_coalesced() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = count.clone();
        let register: ReRegisterFn = Arc::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        });
        let (tx, rx) = mpsc::channel(1);
        let task = tokio::spawn(run_re_registration_task(
            rx,
            Arc::new(RwLock::new(HashMap::from([("test".into(), register)]))),
        ));
        for _ in 0..20 {
            let _ = tx.try_send(());
        }
        drop(tx);
        task.await.unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn connection_route_plan_composite_with_iroh_peer_includes_lan_and_iroh() {
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };
        let plan = connection_route_plan(true, true, Some(&scope));
        assert_eq!(plan.len(), 2);
        assert_eq!(
            plan[0],
            (MeshTransportKind::Lan, MeshScopeId::lan_default(), 100)
        );
        assert_eq!(plan[1], (MeshTransportKind::Iroh, scope, 70));
    }

    #[test]
    fn connection_route_plan_iroh_only_without_scope_adds_no_lan() {
        let plan = connection_route_plan(false, true, None);
        assert!(plan.is_empty());
    }

    #[test]
    fn peer_id_from_multiaddr_extracts_p2p_component() {
        let peer_id = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/1/p2p/{peer_id}")
            .parse()
            .unwrap();
        assert_eq!(peer_id_from_multiaddr(&addr), Some(peer_id));
        assert_eq!(
            peer_id_from_multiaddr(&"/ip4/127.0.0.1/tcp/1".parse().unwrap()),
            None
        );
    }

    #[test]
    fn reconnect_backoff_duration_caps_at_five_minutes() {
        assert_eq!(reconnect_backoff_duration(0), Duration::from_secs(1));
        assert_eq!(reconnect_backoff_duration(1), Duration::from_secs(2));
        assert_eq!(reconnect_backoff_duration(5), Duration::from_secs(32));
        assert_eq!(reconnect_backoff_duration(8), Duration::from_secs(256));
        assert_eq!(
            reconnect_backoff_duration(u32::MAX),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn should_dial_peer_command_requires_iroh_and_scope_for_manual_and_reconnect() {
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let mut scopes = HashMap::new();

        assert!(!should_dial_peer_command(
            &peer,
            DialReason::Manual,
            &scopes,
            false
        ));
        assert!(should_dial_peer_command(
            &peer,
            DialReason::Admission,
            &scopes,
            true,
        ));
        assert!(should_dial_peer_command(
            &peer,
            DialReason::ExistingMeshPeer,
            &scopes,
            true,
        ));
        assert!(!should_dial_peer_command(
            &peer,
            DialReason::Reconnect,
            &scopes,
            true,
        ));

        scopes.insert(
            peer,
            MeshScopeId::Iroh {
                mesh_id: "mesh-a".to_string(),
            },
        );
        assert!(should_dial_peer_command(
            &peer,
            DialReason::Reconnect,
            &scopes,
            true,
        ));
        assert!(should_dial_peer_command(
            &peer,
            DialReason::Manual,
            &scopes,
            true
        ));
    }

    #[test]
    fn seed_scoped_dial_peer_tracks_iroh_peers_by_scope() {
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        let scope = MeshScopeId::Iroh {
            mesh_id: "mesh-a".to_string(),
        };
        let mut by_scope = HashMap::new();
        let mut peer_scopes = HashMap::new();

        seed_scoped_dial_peer(peer, Some(scope.clone()), &mut by_scope, &mut peer_scopes);
        seed_scoped_dial_peer(
            peer,
            Some(MeshScopeId::lan_default()),
            &mut by_scope,
            &mut peer_scopes,
        );

        assert_eq!(peer_scopes.get(&peer), Some(&scope));
        assert!(
            by_scope
                .get("mesh-a")
                .is_some_and(|peers| peers.contains(&peer))
        );
        assert_eq!(by_scope.len(), 1);
    }
}
