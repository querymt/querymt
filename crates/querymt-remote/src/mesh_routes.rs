use crate::{MeshScopeId, MeshTransportKind};
use libp2p::{Multiaddr, PeerId};
use moka::sync::Cache;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RouteKey {
    peer_id: PeerId,
    transport: MeshTransportKind,
    scope: MeshScopeId,
}

#[derive(Debug, Clone)]
pub struct MeshRoute {
    pub peer_id: PeerId,
    pub transport: MeshTransportKind,
    pub scope: MeshScopeId,
    pub addrs: HashSet<Multiaddr>,
    pub last_seen: Instant,
    pub priority: u32,
}

#[derive(Clone, Debug)]
pub struct RouteTable {
    routes: Cache<RouteKey, MeshRoute>,
    // Established connections outlive discovery TTLs and are removed by connection events.
    connected_routes: Arc<RwLock<HashMap<RouteKey, MeshRoute>>>,
}

impl RouteTable {
    fn route_sort_key(route: &MeshRoute) -> (u32, Instant) {
        (route.priority, route.last_seen)
    }

    pub fn new(ttl: Duration) -> Self {
        Self {
            routes: Cache::builder().time_to_idle(ttl).build(),
            connected_routes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn upsert_addrs<I>(
        &self,
        peer_id: PeerId,
        transport: MeshTransportKind,
        scope: MeshScopeId,
        addrs: I,
        priority: u32,
    ) -> MeshRoute
    where
        I: IntoIterator<Item = Multiaddr>,
    {
        let key = RouteKey {
            peer_id,
            transport,
            scope,
        };
        let connected = self.connected_routes.read().get(&key).cloned();
        let mut route = connected
            .or_else(|| self.routes.get(&key))
            .unwrap_or(MeshRoute {
                peer_id,
                transport,
                scope: key.scope.clone(),
                addrs: HashSet::new(),
                last_seen: Instant::now(),
                priority,
            });
        for addr in addrs {
            route.addrs.insert(addr);
        }
        route.last_seen = Instant::now();
        route.priority = priority;
        self.routes.insert(key.clone(), route.clone());
        if let Some(connected) = self.connected_routes.write().get_mut(&key) {
            *connected = route.clone();
        }
        route
    }

    pub(crate) fn connection_established(
        &self,
        peer_id: PeerId,
        transport: MeshTransportKind,
        scope: MeshScopeId,
        addr: Multiaddr,
        priority: u32,
    ) -> MeshRoute {
        let route = self.upsert_addrs(peer_id, transport, scope.clone(), [addr], priority);
        self.connected_routes.write().insert(
            RouteKey {
                peer_id,
                transport,
                scope,
            },
            route.clone(),
        );
        route
    }

    pub fn remove_addrs(
        &self,
        peer_id: PeerId,
        transport: MeshTransportKind,
        scope: MeshScopeId,
        expired: &HashSet<Multiaddr>,
    ) -> Option<MeshRoute> {
        let key = RouteKey {
            peer_id,
            transport,
            scope,
        };
        let mut route = self.routes.get(&key)?;
        for addr in expired {
            route.addrs.remove(addr);
        }
        if route.addrs.is_empty() {
            self.routes.remove(&key);
            None
        } else {
            route.last_seen = Instant::now();
            self.routes.insert(key, route.clone());
            Some(route)
        }
    }

    pub fn routes_for_peer(&self, peer_id: &PeerId) -> Vec<MeshRoute> {
        let mut routes: HashMap<RouteKey, MeshRoute> = self
            .routes
            .iter()
            .filter_map(|(k, v)| {
                if &k.peer_id == peer_id {
                    Some(((*k).clone(), v))
                } else {
                    None
                }
            })
            .collect();
        routes.extend(
            self.connected_routes
                .read()
                .iter()
                .filter(|(key, _)| &key.peer_id == peer_id)
                .map(|(key, route)| (key.clone(), route.clone())),
        );
        routes.into_values().collect()
    }

    pub fn remove_peer(&self, peer_id: &PeerId) {
        self.connected_routes
            .write()
            .retain(|key, _| &key.peer_id != peer_id);
        let keys: Vec<std::sync::Arc<RouteKey>> = self
            .routes
            .iter()
            .filter_map(|(k, _)| if &k.peer_id == peer_id { Some(k) } else { None })
            .collect();
        for k in keys {
            self.routes.remove(k.as_ref());
        }
    }

    pub(crate) fn remove_transport(
        &self,
        peer_id: &PeerId,
        transport: MeshTransportKind,
    ) -> Vec<MeshRoute> {
        let removed: Vec<_> = self
            .routes_for_peer(peer_id)
            .into_iter()
            .filter(|route| route.transport == transport)
            .collect();
        self.connected_routes
            .write()
            .retain(|key, _| &key.peer_id != peer_id || key.transport != transport);
        for route in &removed {
            self.routes.remove(&RouteKey {
                peer_id: *peer_id,
                transport,
                scope: route.scope.clone(),
            });
        }
        removed
    }

    pub(crate) fn remove_scope(&self, scope: &MeshScopeId) {
        self.connected_routes
            .write()
            .retain(|key, _| &key.scope != scope);
        let keys: Vec<_> = self
            .routes
            .iter()
            .filter_map(|(key, _)| (&key.scope == scope).then_some(key))
            .collect();
        for key in keys {
            self.routes.remove(key.as_ref());
        }
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.connected_routes
            .read()
            .keys()
            .any(|key| &key.peer_id == peer_id)
            || self.routes.iter().any(|(k, _)| &k.peer_id == peer_id)
    }

    pub fn peer_ids(&self) -> Vec<PeerId> {
        let mut out: HashSet<PeerId> = self
            .connected_routes
            .read()
            .keys()
            .map(|key| key.peer_id)
            .collect();
        for (k, _) in self.routes.iter() {
            out.insert(k.peer_id);
        }
        out.into_iter().collect()
    }

    pub fn peer_count(&self) -> usize {
        self.peer_ids().len()
    }

    pub fn best_route_for_peer(&self, peer_id: &PeerId) -> Option<MeshRoute> {
        self.routes_for_peer(peer_id)
            .into_iter()
            .max_by_key(Self::route_sort_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_id() -> PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    fn addr(value: String) -> Multiaddr {
        value.parse().unwrap()
    }

    #[test]
    fn connected_routes_survive_discovery_ttl_and_address_expiry() {
        let table = RouteTable::new(Duration::from_millis(10));
        let peer = peer_id();
        let address = addr(format!("/ip4/127.0.0.1/tcp/1000/p2p/{peer}"));
        table.connection_established(
            peer,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            address.clone(),
            100,
        );
        std::thread::sleep(Duration::from_millis(50));
        table.remove_addrs(
            peer,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            &HashSet::from([address]),
        );
        assert!(table.is_peer_alive(&peer));
        assert_eq!(table.peer_ids(), vec![peer]);
        assert!(table.best_route_for_peer(&peer).is_some());
        table.remove_peer(&peer);
        assert!(!table.is_peer_alive(&peer));
    }

    #[test]
    fn closing_one_transport_preserves_the_other_and_scope_removal_clears_pins() {
        let table = RouteTable::new(Duration::from_secs(90));
        let peer = peer_id();
        let scope = MeshScopeId::Iroh {
            mesh_id: "a".into(),
        };
        table.connection_established(
            peer,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            addr(format!("/ip4/127.0.0.1/tcp/1/p2p/{peer}")),
            100,
        );
        table.connection_established(
            peer,
            MeshTransportKind::Iroh,
            scope.clone(),
            addr(format!("/p2p/{peer}")),
            70,
        );
        assert_eq!(
            table.remove_transport(&peer, MeshTransportKind::Lan).len(),
            1
        );
        assert!(table.is_peer_alive(&peer));
        assert_eq!(
            table.best_route_for_peer(&peer).unwrap().transport,
            MeshTransportKind::Iroh
        );
        table.remove_scope(&scope);
        assert!(!table.is_peer_alive(&peer));
    }

    #[test]
    fn upsert_merges_addresses_for_same_route_key() {
        let table = RouteTable::new(Duration::from_secs(60));
        let peer = peer_id();
        let scope = MeshScopeId::lan_default();

        table.upsert_addrs(
            peer,
            MeshTransportKind::Lan,
            scope.clone(),
            [addr(format!("/ip4/127.0.0.1/tcp/1000/p2p/{peer}"))],
            10,
        );
        let route = table.upsert_addrs(
            peer,
            MeshTransportKind::Lan,
            scope,
            [addr(format!("/ip4/127.0.0.1/tcp/2000/p2p/{peer}"))],
            20,
        );

        assert_eq!(route.priority, 20);
        assert_eq!(route.addrs.len(), 2);
        assert_eq!(table.routes_for_peer(&peer).len(), 1);
    }

    #[test]
    fn remove_addrs_prunes_route_when_last_address_is_removed() {
        let table = RouteTable::new(Duration::from_secs(60));
        let peer = peer_id();
        let scope = MeshScopeId::lan_default();
        let remove = addr(format!("/ip4/127.0.0.1/tcp/1000/p2p/{peer}"));

        table.upsert_addrs(
            peer,
            MeshTransportKind::Lan,
            scope.clone(),
            [remove.clone()],
            10,
        );

        let expired = HashSet::from([remove]);
        assert!(
            table
                .remove_addrs(peer, MeshTransportKind::Lan, scope, &expired)
                .is_none()
        );
        assert!(!table.is_peer_alive(&peer));
    }

    #[test]
    fn remove_peer_clears_all_routes_for_that_peer_only() {
        let table = RouteTable::new(Duration::from_secs(60));
        let peer_a = peer_id();
        let peer_b = peer_id();

        table.upsert_addrs(
            peer_a,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            [addr(format!("/ip4/127.0.0.1/tcp/1000/p2p/{peer_a}"))],
            10,
        );
        table.upsert_addrs(
            peer_b,
            MeshTransportKind::Iroh,
            MeshScopeId::Iroh {
                mesh_id: "mesh-a".to_string(),
            },
            [addr(format!("/p2p/{peer_b}"))],
            20,
        );

        table.remove_peer(&peer_a);

        assert!(!table.is_peer_alive(&peer_a));
        assert!(table.is_peer_alive(&peer_b));
        assert_eq!(table.peer_count(), 1);
    }

    #[test]
    fn best_route_prefers_highest_priority() {
        let table = RouteTable::new(Duration::from_secs(60));
        let peer = peer_id();

        table.upsert_addrs(
            peer,
            MeshTransportKind::Iroh,
            MeshScopeId::Iroh {
                mesh_id: "mesh-a".to_string(),
            },
            [addr(format!("/p2p/{peer}"))],
            50,
        );
        table.upsert_addrs(
            peer,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            [addr(format!("/ip4/127.0.0.1/tcp/1000/p2p/{peer}"))],
            100,
        );

        let best = table.best_route_for_peer(&peer).unwrap();
        assert_eq!(best.transport, MeshTransportKind::Lan);
        assert_eq!(best.priority, 100);
    }

    #[test]
    fn peer_ids_are_unique_across_multiple_routes() {
        let table = RouteTable::new(Duration::from_secs(60));
        let peer = peer_id();

        table.upsert_addrs(
            peer,
            MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            [addr(format!("/ip4/127.0.0.1/tcp/1000/p2p/{peer}"))],
            100,
        );
        table.upsert_addrs(
            peer,
            MeshTransportKind::Iroh,
            MeshScopeId::Iroh {
                mesh_id: "mesh-a".to_string(),
            },
            [addr(format!("/p2p/{peer}"))],
            50,
        );

        let mut peers = table.peer_ids();
        peers.sort();
        assert_eq!(peers, vec![peer]);
        assert_eq!(table.peer_count(), 1);
    }
}
