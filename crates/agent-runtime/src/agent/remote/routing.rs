//! Routing table actor — single owner of per-agent routing policy.
//!
//! The `RoutingActor` manages a `HashMap<String, RoutingPolicy>` and publishes
//! immutable snapshots to an `ArcSwap` on every mutation. Decision points
//! (orchestrator, session creation) pull from the snapshot at decision time.
//!
//! ## Peer lifecycle
//!
//! The actor subscribes to `PeerEvent` broadcast:
//! - `Discovered(peer_id)` → eagerly resolves pending `Peer(name)` targets
//! - `Expired(peer_id)` → marks affected routes as unresolved
//!
//! ## Messages
//!
//! - `SetSessionTarget` — set where a delegate's session runs (local or remote peer)
//! - `SetProviderTarget` — set which peer's LLM to use (local or remote peer)
//! - `ClearRoute` — remove routing policy for an agent
//! - `ListRoutes` — return the current snapshot

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::Arc;

// ── Value types ──────────────────────────────────────────────────────────

/// Where work is directed — local or to a named peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteTarget {
    /// Run locally on this node.
    Local,
    /// Route to a named peer (human-readable name from config).
    Peer(String),
}

/// Per-agent routing policy.
///
/// Describes where an agent's delegation sessions and LLM provider calls go.
/// When `session_target = Peer(X)`, `provider_target` is irrelevant (the remote
/// peer handles its own provider).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingPolicy {
    /// Whether the entire delegation session runs locally or on a remote peer.
    pub session_target: RouteTarget,
    /// Which peer's LLM is used when the session runs locally.
    pub provider_target: RouteTarget,
    /// Eagerly resolved from `provider_target` when the peer is discovered.
    /// `None` = not yet resolved or target is `Local`.
    pub resolved_provider_node_id: Option<String>,
}

impl RoutingPolicy {
    /// Create a policy that runs everything locally.
    pub fn local() -> Self {
        Self {
            session_target: RouteTarget::Local,
            provider_target: RouteTarget::Local,
            resolved_provider_node_id: None,
        }
    }

    /// Create a policy that routes the provider to a named peer.
    pub fn with_provider_peer(peer_name: impl Into<String>) -> Self {
        Self {
            session_target: RouteTarget::Local,
            provider_target: RouteTarget::Peer(peer_name.into()),
            resolved_provider_node_id: None,
        }
    }

    /// Create a policy that routes the entire session to a named peer.
    pub fn with_session_peer(peer_name: impl Into<String>) -> Self {
        Self {
            session_target: RouteTarget::Peer(peer_name.into()),
            provider_target: RouteTarget::Local,
            resolved_provider_node_id: None,
        }
    }
}

/// Immutable routing snapshot — held in `ArcSwap` for lock-free reads.
#[derive(Debug, Clone, Default)]
pub struct RoutingSnapshot {
    routes: HashMap<String, RoutingPolicy>,
}

impl RoutingSnapshot {
    /// Look up the routing policy for a given agent ID.
    pub fn get(&self, agent_id: &str) -> Option<&RoutingPolicy> {
        self.routes.get(agent_id)
    }

    /// Iterate over all routes.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &RoutingPolicy)> {
        self.routes.iter()
    }

    /// Number of routing entries.
    pub fn len(&self) -> usize {
        self.routes.len()
    }

    /// Whether the snapshot is empty.
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Shared handle for lock-free reads of the current routing snapshot.
pub type RoutingSnapshotHandle = Arc<ArcSwap<RoutingSnapshot>>;

/// Create a new empty routing snapshot handle.
pub fn new_routing_snapshot_handle() -> RoutingSnapshotHandle {
    Arc::new(ArcSwap::from_pointee(RoutingSnapshot::default()))
}

// ── Messages ─────────────────────────────────────────────────────────────

/// Set the session target for an agent (Local or Peer).
#[derive(Debug, Clone)]
pub struct SetSessionTarget {
    pub agent_id: String,
    pub target: RouteTarget,
}

/// Set the provider target for an agent (Local or Peer).
#[derive(Debug, Clone)]
pub struct SetProviderTarget {
    pub agent_id: String,
    pub target: RouteTarget,
}

/// Remove routing policy for an agent.
#[derive(Debug, Clone)]
pub struct ClearRoute {
    pub agent_id: String,
}

/// List all current routes (returns the snapshot).
#[derive(Debug, Clone)]
pub struct ListRoutes;

/// Resolve a pending peer name to a node ID.
/// Sent internally when a peer is discovered.
#[derive(Debug, Clone)]
pub struct ResolvePeer {
    pub peer_name: String,
    pub node_id: String,
}

/// Mark all routes resolved to a given node ID as unresolved.
/// Sent internally when a peer expires.
#[derive(Debug, Clone)]
pub struct UnresolvePeer {
    pub node_id: String,
}

// ── Response types ───────────────────────────────────────────────────────

/// Confirmation returned by route-mutating messages.
#[derive(Debug, Clone, Reply)]
pub struct RouteConfirmation {
    pub agent_id: String,
    pub policy: Option<RoutingPolicy>,
}

// ── Actor ────────────────────────────────────────────────────────────────

use kameo::Actor;
use kameo::Reply;
use kameo::message::{Context, Message};

/// The routing actor — single owner/writer of routing state.
///
/// Holds a `HashMap<String, RoutingPolicy>` as authoritative state.
/// On every mutation, publishes an immutable snapshot to `ArcSwap`.
#[derive(Actor)]
pub struct RoutingActor {
    routes: HashMap<String, RoutingPolicy>,
    snapshot: RoutingSnapshotHandle,
}

impl RoutingActor {
    /// Create a new routing actor with the given snapshot handle.
    ///
    /// The snapshot handle should be shared with consumers (orchestrator, etc.)
    /// who need lock-free read access to the routing table.
    pub fn new(snapshot: RoutingSnapshotHandle) -> Self {
        Self {
            routes: HashMap::new(),
            snapshot,
        }
    }

    /// Publish the current routing state as an immutable snapshot.
    fn publish_snapshot(&self) {
        let snap = RoutingSnapshot {
            routes: self.routes.clone(),
        };
        self.snapshot.store(Arc::new(snap));
    }

    /// Get or create a mutable reference to a policy for the given agent.
    fn ensure_policy(&mut self, agent_id: &str) -> &mut RoutingPolicy {
        self.routes
            .entry(agent_id.to_string())
            .or_insert_with(RoutingPolicy::local)
    }
}

// ── Message handlers ─────────────────────────────────────────────────────

impl Message<SetSessionTarget> for RoutingActor {
    type Reply = RouteConfirmation;

    async fn handle(
        &mut self,
        msg: SetSessionTarget,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let policy = self.ensure_policy(&msg.agent_id);
        policy.session_target = msg.target;
        log::info!(
            "RoutingActor: set session_target for '{}' → {:?}",
            msg.agent_id,
            policy.session_target
        );
        let policy_clone = self.routes.get(&msg.agent_id).cloned();
        self.publish_snapshot();
        RouteConfirmation {
            agent_id: msg.agent_id,
            policy: policy_clone,
        }
    }
}

impl Message<SetProviderTarget> for RoutingActor {
    type Reply = RouteConfirmation;

    async fn handle(
        &mut self,
        msg: SetProviderTarget,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let policy = self.ensure_policy(&msg.agent_id);
        // When the target changes, clear the stale resolution.
        if policy.provider_target != msg.target {
            policy.resolved_provider_node_id = None;
        }
        policy.provider_target = msg.target;
        log::info!(
            "RoutingActor: set provider_target for '{}' → {:?}",
            msg.agent_id,
            policy.provider_target
        );
        let policy_clone = self.routes.get(&msg.agent_id).cloned();
        self.publish_snapshot();
        RouteConfirmation {
            agent_id: msg.agent_id,
            policy: policy_clone,
        }
    }
}

impl Message<ClearRoute> for RoutingActor {
    type Reply = RouteConfirmation;

    async fn handle(
        &mut self,
        msg: ClearRoute,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let removed = self.routes.remove(&msg.agent_id);
        log::info!(
            "RoutingActor: cleared route for '{}' (existed: {})",
            msg.agent_id,
            removed.is_some()
        );
        self.publish_snapshot();
        RouteConfirmation {
            agent_id: msg.agent_id,
            policy: None,
        }
    }
}

impl Message<ListRoutes> for RoutingActor {
    type Reply = Arc<RoutingSnapshot>;

    async fn handle(
        &mut self,
        _msg: ListRoutes,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.snapshot.load_full()
    }
}

impl Message<ResolvePeer> for RoutingActor {
    type Reply = usize;

    async fn handle(
        &mut self,
        msg: ResolvePeer,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut resolved_count = 0;
        for (agent_id, policy) in &mut self.routes {
            // Resolve provider_target if it matches the peer name
            if policy.provider_target == RouteTarget::Peer(msg.peer_name.clone())
                && policy.resolved_provider_node_id.is_none()
            {
                policy.resolved_provider_node_id = Some(msg.node_id.clone());
                log::info!(
                    "RoutingActor: resolved provider peer '{}' → node_id={} for agent '{}'",
                    msg.peer_name,
                    msg.node_id,
                    agent_id
                );
                resolved_count += 1;
            }
        }
        if resolved_count > 0 {
            self.publish_snapshot();
        }
        resolved_count
    }
}

impl Message<UnresolvePeer> for RoutingActor {
    type Reply = usize;

    async fn handle(
        &mut self,
        msg: UnresolvePeer,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let mut degraded_count = 0;
        for (agent_id, policy) in &mut self.routes {
            if policy.resolved_provider_node_id.as_deref() == Some(&msg.node_id) {
                policy.resolved_provider_node_id = None;
                log::warn!(
                    "RoutingActor: peer expired — degraded route for agent '{}' (was node_id={})",
                    agent_id,
                    msg.node_id
                );
                degraded_count += 1;
            }
        }
        if degraded_count > 0 {
            self.publish_snapshot();
        }
        degraded_count
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::actor::Spawn;

    /// Helper: create actor + snapshot handle, return both.
    async fn setup() -> (kameo::actor::ActorRef<RoutingActor>, RoutingSnapshotHandle) {
        let handle = new_routing_snapshot_handle();
        let actor = RoutingActor::new(handle.clone());
        let actor_ref = RoutingActor::spawn(actor);
        (actor_ref, handle)
    }

    // ── RED: snapshot starts empty ───────────────────────────────────────

    #[tokio::test]
    async fn snapshot_starts_empty() {
        let (_, snapshot) = setup().await;
        let snap = snapshot.load();
        assert!(snap.is_empty(), "fresh snapshot should be empty");
    }

    // ── RED: SetProviderTarget creates entry and updates snapshot ─────────

    #[tokio::test]
    async fn set_provider_target_creates_entry_and_updates_snapshot() {
        let (actor, snapshot) = setup().await;
        let confirmation = actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .expect("ask SetProviderTarget");

        assert_eq!(confirmation.agent_id, "coder");
        let policy = confirmation.policy.expect("policy should exist");
        assert_eq!(policy.provider_target, RouteTarget::Peer("gpu-box".into()));
        assert_eq!(policy.session_target, RouteTarget::Local);

        // Verify snapshot was updated
        let snap = snapshot.load();
        let entry = snap.get("coder").expect("snapshot should have 'coder'");
        assert_eq!(entry.provider_target, RouteTarget::Peer("gpu-box".into()));
    }

    // ── RED: SetSessionTarget creates entry and updates snapshot ──────────

    #[tokio::test]
    async fn set_session_target_creates_entry_and_updates_snapshot() {
        let (actor, snapshot) = setup().await;
        let confirmation = actor
            .ask(SetSessionTarget {
                agent_id: "reviewer".into(),
                target: RouteTarget::Peer("nas-node".into()),
            })
            .await
            .expect("ask SetSessionTarget");

        assert_eq!(confirmation.agent_id, "reviewer");
        let policy = confirmation.policy.expect("policy should exist");
        assert_eq!(policy.session_target, RouteTarget::Peer("nas-node".into()));

        let snap = snapshot.load();
        let entry = snap
            .get("reviewer")
            .expect("snapshot should have 'reviewer'");
        assert_eq!(entry.session_target, RouteTarget::Peer("nas-node".into()));
    }

    // ── RED: ClearRoute removes entry and updates snapshot ───────────────

    #[tokio::test]
    async fn clear_route_removes_entry() {
        let (actor, snapshot) = setup().await;

        // Add a route first
        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu".into()),
            })
            .await
            .expect("setup");

        assert!(!snapshot.load().is_empty());

        // Clear it
        let confirmation = actor
            .ask(ClearRoute {
                agent_id: "coder".into(),
            })
            .await
            .expect("ask ClearRoute");

        assert_eq!(confirmation.agent_id, "coder");
        assert!(confirmation.policy.is_none());

        let snap = snapshot.load();
        assert!(snap.is_empty(), "snapshot should be empty after ClearRoute");
    }

    // ── RED: ListRoutes returns current snapshot ─────────────────────────

    #[tokio::test]
    async fn list_routes_returns_current_snapshot() {
        let (actor, _) = setup().await;

        actor
            .ask(SetProviderTarget {
                agent_id: "a".into(),
                target: RouteTarget::Peer("p1".into()),
            })
            .await
            .unwrap();
        actor
            .ask(SetSessionTarget {
                agent_id: "b".into(),
                target: RouteTarget::Peer("p2".into()),
            })
            .await
            .unwrap();

        let snap = actor.ask(ListRoutes).await.expect("ask ListRoutes");
        assert_eq!(snap.len(), 2);
        assert!(snap.get("a").is_some());
        assert!(snap.get("b").is_some());
    }

    // ── RED: ResolvePeer eagerly resolves matching routes ─────────────────

    #[tokio::test]
    async fn resolve_peer_resolves_matching_routes() {
        let (actor, snapshot) = setup().await;

        // Set up two agents pointing to "gpu-box"
        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .unwrap();
        actor
            .ask(SetProviderTarget {
                agent_id: "reviewer".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .unwrap();
        // One agent pointing elsewhere
        actor
            .ask(SetProviderTarget {
                agent_id: "tester".into(),
                target: RouteTarget::Peer("other".into()),
            })
            .await
            .unwrap();

        // Resolve "gpu-box" → some node ID
        let count = actor
            .ask(ResolvePeer {
                peer_name: "gpu-box".into(),
                node_id: "12D3KooWGPU".into(),
            })
            .await
            .expect("ask ResolvePeer");

        assert_eq!(count, 2, "should resolve exactly 2 routes");

        let snap = snapshot.load();
        assert_eq!(
            snap.get("coder").unwrap().resolved_provider_node_id,
            Some("12D3KooWGPU".into())
        );
        assert_eq!(
            snap.get("reviewer").unwrap().resolved_provider_node_id,
            Some("12D3KooWGPU".into())
        );
        assert_eq!(
            snap.get("tester").unwrap().resolved_provider_node_id,
            None,
            "unrelated route should not be resolved"
        );
    }

    // ── RED: ResolvePeer skips already-resolved routes ────────────────────

    #[tokio::test]
    async fn resolve_peer_skips_already_resolved() {
        let (actor, snapshot) = setup().await;

        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .unwrap();

        // First resolution
        actor
            .ask(ResolvePeer {
                peer_name: "gpu-box".into(),
                node_id: "node-1".into(),
            })
            .await
            .unwrap();

        // Second resolution (different node_id) — should NOT overwrite
        let count = actor
            .ask(ResolvePeer {
                peer_name: "gpu-box".into(),
                node_id: "node-2".into(),
            })
            .await
            .unwrap();

        assert_eq!(count, 0, "already-resolved route should be skipped");

        let snap = snapshot.load();
        assert_eq!(
            snap.get("coder").unwrap().resolved_provider_node_id,
            Some("node-1".into()),
            "original resolution should be preserved"
        );
    }

    // ── RED: UnresolvePeer degrades matching routes ──────────────────────

    #[tokio::test]
    async fn unresolve_peer_degrades_matching_routes() {
        let (actor, snapshot) = setup().await;

        // Set up and resolve
        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .unwrap();
        actor
            .ask(SetProviderTarget {
                agent_id: "reviewer".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .unwrap();
        actor
            .ask(ResolvePeer {
                peer_name: "gpu-box".into(),
                node_id: "12D3KooWGPU".into(),
            })
            .await
            .unwrap();

        // Verify resolved
        assert!(
            snapshot
                .load()
                .get("coder")
                .unwrap()
                .resolved_provider_node_id
                .is_some()
        );

        // Unresolve
        let count = actor
            .ask(UnresolvePeer {
                node_id: "12D3KooWGPU".into(),
            })
            .await
            .expect("ask UnresolvePeer");

        assert_eq!(count, 2, "should degrade exactly 2 routes");

        let snap = snapshot.load();
        assert_eq!(
            snap.get("coder").unwrap().resolved_provider_node_id,
            None,
            "resolved_provider_node_id should be cleared"
        );
        assert_eq!(
            snap.get("reviewer").unwrap().resolved_provider_node_id,
            None,
        );
    }

    // ── RED: Peer lifecycle: resolve → unresolve → re-resolve ────────────

    #[tokio::test]
    async fn peer_lifecycle_resolve_unresolve_reresolve() {
        let (actor, snapshot) = setup().await;

        // Set up route
        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .unwrap();

        // Discovered: resolve
        actor
            .ask(ResolvePeer {
                peer_name: "gpu-box".into(),
                node_id: "node-A".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .load()
                .get("coder")
                .unwrap()
                .resolved_provider_node_id,
            Some("node-A".into())
        );

        // Expired: unresolve
        actor
            .ask(UnresolvePeer {
                node_id: "node-A".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .load()
                .get("coder")
                .unwrap()
                .resolved_provider_node_id,
            None
        );

        // Re-discovered: re-resolve (now with different node ID — peer got new identity)
        let count = actor
            .ask(ResolvePeer {
                peer_name: "gpu-box".into(),
                node_id: "node-B".into(),
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            snapshot
                .load()
                .get("coder")
                .unwrap()
                .resolved_provider_node_id,
            Some("node-B".into())
        );
    }

    // ── RED: Multiple agents with different targets ──────────────────────

    #[tokio::test]
    async fn multiple_agents_independent_routing() {
        let (actor, snapshot) = setup().await;

        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-box".into()),
            })
            .await
            .unwrap();
        actor
            .ask(SetSessionTarget {
                agent_id: "reviewer".into(),
                target: RouteTarget::Peer("nas".into()),
            })
            .await
            .unwrap();
        actor
            .ask(SetProviderTarget {
                agent_id: "tester".into(),
                target: RouteTarget::Local,
            })
            .await
            .unwrap();

        let snap = snapshot.load();
        assert_eq!(snap.len(), 3);

        // coder: provider=Peer, session=Local
        let coder = snap.get("coder").unwrap();
        assert_eq!(coder.provider_target, RouteTarget::Peer("gpu-box".into()));
        assert_eq!(coder.session_target, RouteTarget::Local);

        // reviewer: session=Peer, provider=Local (default)
        let reviewer = snap.get("reviewer").unwrap();
        assert_eq!(reviewer.session_target, RouteTarget::Peer("nas".into()));
        assert_eq!(reviewer.provider_target, RouteTarget::Local);

        // tester: everything local
        let tester = snap.get("tester").unwrap();
        assert_eq!(tester.provider_target, RouteTarget::Local);
        assert_eq!(tester.session_target, RouteTarget::Local);
    }

    // ── RED: SetProviderTarget overwrites previous target ────────────────

    #[tokio::test]
    async fn set_provider_target_overwrites_previous() {
        let (actor, snapshot) = setup().await;

        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-1".into()),
            })
            .await
            .unwrap();

        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu-2".into()),
            })
            .await
            .unwrap();

        let snap = snapshot.load();
        assert_eq!(
            snap.get("coder").unwrap().provider_target,
            RouteTarget::Peer("gpu-2".into()),
            "second SetProviderTarget should overwrite the first"
        );
    }

    // ── RED: SetProviderTarget to Local clears resolved node ID ──────────

    #[tokio::test]
    async fn set_provider_target_to_local_clears_resolution() {
        let (actor, snapshot) = setup().await;

        // Set peer, resolve it
        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Peer("gpu".into()),
            })
            .await
            .unwrap();
        actor
            .ask(ResolvePeer {
                peer_name: "gpu".into(),
                node_id: "node-1".into(),
            })
            .await
            .unwrap();
        assert!(
            snapshot
                .load()
                .get("coder")
                .unwrap()
                .resolved_provider_node_id
                .is_some()
        );

        // Switch back to Local
        actor
            .ask(SetProviderTarget {
                agent_id: "coder".into(),
                target: RouteTarget::Local,
            })
            .await
            .unwrap();

        let snap = snapshot.load();
        let policy = snap.get("coder").unwrap();
        assert_eq!(policy.provider_target, RouteTarget::Local);
        // resolved_provider_node_id should be cleared when target changes
        // (the current resolution was for the old peer)
        assert_eq!(
            policy.resolved_provider_node_id, None,
            "resolved_provider_node_id should be cleared when target changes"
        );
    }
}
