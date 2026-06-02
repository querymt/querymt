use libp2p::{Multiaddr, PeerId};
use moka::sync::Cache;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::super::scope::{MeshScopeId, MeshTransportKind};

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
