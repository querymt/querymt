use kameo::remote;
use libp2p::{Multiaddr, PeerId};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::broadcast;

use super::handle::ReRegisterFn;
use super::{PeerEvent, RouteTable};
use querymt_remote::{MeshScopeId, MeshStateStore, MeshTransportKind, PeerEntry};

/// Handle a `ConnectionEstablished` swarm event.
///
/// Populates Kademlia's routing table, updates `known_peers`, fires
/// `PeerEvent::Discovered` for genuinely new peers, and triggers the
/// re-registration cascade so the new peer's DHT is populated immediately.
pub(super) fn handle_mdns_discovered<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    list: Vec<(PeerId, Multiaddr)>,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    routes: &RouteTable,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
    re_register_fns: &RwLock<HashMap<String, ReRegisterFn>>,
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

        let route = routes.upsert_addrs(
            peer_id,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            new_addrs.clone(),
            100,
        );
        let _ = peer_events_tx.send(PeerEvent::RouteAdded { peer_id, route });
        let _ = peer_events_tx.send(PeerEvent::Discovered(peer_id));

        let fns: Vec<ReRegisterFn> = re_register_fns.read().values().cloned().collect();
        if !fns.is_empty() {
            tokio::spawn(async move {
                for f in &fns {
                    f().await;
                }
            });
        }
    }
}

pub(super) fn handle_mdns_expired<B: libp2p::swarm::NetworkBehaviour>(
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
        if let Some(route) = routes.remove_addrs(
            peer_id,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            &expired_set,
        ) {
            let _ = peer_events_tx.send(PeerEvent::RouteRemoved { peer_id, route });
        }

        if peer_fully_gone {
            log::info!("mDNS peer expired (went away): {peer_id}");
            let _ = swarm.disconnect_peer_id(peer_id);
            let _ = peer_events_tx.send(PeerEvent::Expired(peer_id));
        } else {
            log::debug!(
                "mDNS partial expiry for peer {peer_id}: some addresses expired but peer still reachable"
            );
        }
    }
}

/// Determine which routes should be refreshed on a connection-established event.
pub(super) fn connection_route_plan(
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

pub(super) fn refresh_mesh_state_known_peers(
    mesh_state_store: &Option<Arc<RwLock<MeshStateStore>>>,
    routes: &RouteTable,
) {
    let Some(ms) = mesh_state_store.as_ref() else {
        return;
    };

    let peers: Vec<PeerEntry> = routes
        .peer_ids()
        .into_iter()
        .map(|pid| PeerEntry {
            peer_id: pid.to_string(),
            addrs: vec![format!("/p2p/{pid}")],
        })
        .collect();

    let ms = Arc::clone(ms);
    tokio::spawn(async move {
        let mut store = ms.write();
        let mesh_ids = store.active_mesh_ids();
        for mid in mesh_ids {
            let _ = store.update_known_peers(&mid, peers.clone());
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_connection_established<B: libp2p::swarm::NetworkBehaviour>(
    swarm: &mut libp2p::Swarm<B>,
    peer_id: PeerId,
    remote_addr: Multiaddr,
    routes: &RouteTable,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
    re_register_fns: &RwLock<HashMap<String, ReRegisterFn>>,
    transport: MeshTransportKind,
    scope: MeshScopeId,
    priority: u32,
) {
    swarm.add_peer_address(peer_id, remote_addr.clone());

    let was_alive = routes.is_peer_alive(&peer_id);
    let route = routes.upsert_addrs(peer_id, transport, scope, [remote_addr.clone()], priority);
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

        let fns: Vec<ReRegisterFn> = re_register_fns.read().values().cloned().collect();
        if !fns.is_empty() {
            tokio::spawn(async move {
                for f in &fns {
                    f().await;
                }
            });
        }
    } else {
        log::debug!("Additional connection to already-known peer {peer_id} at {remote_addr}");
    }
}

/// Handle a `ConnectionClosed` swarm event.
pub(super) fn handle_connection_closed(
    peer_id: PeerId,
    num_established: u32,
    routes: &RouteTable,
    known_peers: &RwLock<HashMap<PeerId, HashSet<Multiaddr>>>,
    peer_events_tx: &broadcast::Sender<PeerEvent>,
) {
    if num_established == 0 {
        let removed_routes = routes.routes_for_peer(&peer_id);
        routes.remove_peer(&peer_id);
        for route in removed_routes {
            let _ = peer_events_tx.send(PeerEvent::RouteRemoved { peer_id, route });
        }

        let was_known = {
            let mut peers = known_peers.write();
            peers.remove(&peer_id).is_some()
        };
        if was_known {
            log::info!("Peer disconnected (no remaining connections): {peer_id}");
            let _ = peer_events_tx.send(PeerEvent::Expired(peer_id));
        }
    }
}

pub(super) fn peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    use libp2p::multiaddr::Protocol;

    addr.iter().find_map(|p| match p {
        Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}

pub(super) fn reconnect_backoff_duration(attempt: u32) -> std::time::Duration {
    let secs = (1u64 << attempt.min(5)).min(30);
    std::time::Duration::from_secs(secs)
}

pub(super) fn should_track_iroh_reconnect(
    peer_id: &PeerId,
    peer_iroh_scope: &HashMap<PeerId, MeshScopeId>,
) -> bool {
    peer_iroh_scope.contains_key(peer_id)
}

pub(super) fn should_dial_peer_command(
    peer_id: &PeerId,
    reason: super::DialReason,
    peer_iroh_scope: &HashMap<PeerId, MeshScopeId>,
    has_iroh: bool,
) -> bool {
    if !has_iroh {
        return false;
    }
    match reason {
        super::DialReason::Admission | super::DialReason::ExistingMeshPeer => true,
        super::DialReason::Reconnect | super::DialReason::Manual => {
            should_track_iroh_reconnect(peer_id, peer_iroh_scope)
        }
    }
}

pub(super) fn seed_scoped_dial_peer(
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

pub(super) fn log_kameo_messaging_event(event: &remote::messaging::Event) {
    match event {
        remote::messaging::Event::AskResult {
            peer,
            connection_id,
            request_id,
            result,
        } => match result {
            Ok(_) => tracing::debug!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                "kameo ask completed"
            ),
            Err(error) => tracing::warn!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                error = ?error,
                "kameo ask failed"
            ),
        },
        remote::messaging::Event::TellResult {
            peer,
            connection_id,
            request_id,
            result,
        } => match result {
            Ok(()) => tracing::debug!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                "kameo tell acknowledged"
            ),
            Err(error) => tracing::warn!(
                target: "remote::mesh::messaging",
                peer = %peer,
                connection_id = ?connection_id,
                request_id = %request_id,
                error = %error,
                "kameo tell failed"
            ),
        },
        remote::messaging::Event::LinkResult {
            peer,
            connection_id,
            request_id,
            result,
        } => tracing::debug!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = ?connection_id,
            request_id = %request_id,
            ok = result.is_ok(),
            "kameo link result"
        ),
        remote::messaging::Event::UnlinkResult {
            peer,
            connection_id,
            request_id,
            result,
        } => tracing::debug!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = ?connection_id,
            request_id = %request_id,
            ok = result.is_ok(),
            "kameo unlink result"
        ),
        remote::messaging::Event::SignalLinkDiedResult {
            peer,
            connection_id,
            request_id,
            result,
        } => tracing::debug!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = ?connection_id,
            request_id = %request_id,
            ok = result.is_ok(),
            "kameo signal_link_died result"
        ),
        remote::messaging::Event::OutboundFailure {
            peer,
            connection_id,
            request_id,
            error,
        } => tracing::warn!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = %connection_id,
            request_id = %request_id,
            error = %error,
            "kameo outbound failure"
        ),
        remote::messaging::Event::InboundFailure {
            peer,
            connection_id,
            request_id,
            error,
        } => tracing::warn!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = %connection_id,
            request_id = %request_id,
            error = ?error,
            "kameo inbound failure"
        ),
        remote::messaging::Event::ResponseSent {
            peer,
            connection_id,
            request_id,
        } => tracing::trace!(
            target: "remote::mesh::messaging",
            peer = %peer,
            connection_id = %connection_id,
            request_id = %request_id,
            "kameo response sent"
        ),
    }
}
