use crate::{MeshScopeId, MeshTransportKind};
use libp2p::{Multiaddr, PeerId};
use moka::sync::Cache;
use std::collections::HashSet;
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
}

impl RouteTable {
    fn route_sort_key(route: &MeshRoute) -> (u32, Instant) {
        (route.priority, route.last_seen)
    }

    pub fn new(ttl: Duration) -> Self {
        Self {
            routes: Cache::builder().time_to_idle(ttl).build(),
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
        let mut route = self.routes.get(&key).unwrap_or(MeshRoute {
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
        self.routes.insert(key, route.clone());
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
        self.routes
            .iter()
            .filter_map(|(k, v)| if &k.peer_id == peer_id { Some(v) } else { None })
            .collect()
    }

    pub fn remove_peer(&self, peer_id: &PeerId) {
        let keys: Vec<std::sync::Arc<RouteKey>> = self
            .routes
            .iter()
            .filter_map(|(k, _)| if &k.peer_id == peer_id { Some(k) } else { None })
            .collect();
        for k in keys {
            self.routes.remove(k.as_ref());
        }
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.routes.iter().any(|(k, _)| &k.peer_id == peer_id)
    }

    pub fn peer_ids(&self) -> Vec<PeerId> {
        let mut out = HashSet::new();
        for (k, _) in self.routes.iter() {
            out.insert(k.peer_id);
        }
        out.into_iter().collect()
    }

    pub fn peer_count(&self) -> usize {
        self.peer_ids().len()
    }

    pub fn best_route_for_peer(&self, peer_id: &PeerId) -> Option<MeshRoute> {
        self.routes
            .iter()
            .filter_map(|(k, v)| if &k.peer_id == peer_id { Some(v) } else { None })
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
