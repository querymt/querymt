use kameo::remote;
use libp2p::PeerId;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

use super::{DialReason, SwarmCommand};
use super::{MeshEvent, MeshRoute, MeshTransportMode, RouteTable};
use querymt_remote::{
    InviteError, InviteGrant, InvitePermissions, InviteStore, MeshScopeId, MeshStateStore,
    NodeId, PeerEntry, SignedInviteGrant, mesh_id_for, scoped_node_manager_for_peer,
};

/// Type alias for a boxed re-registration closure.
///
/// Each closure captures one (`ActorRef`, `name`) pair and re-runs the full
/// `into_remote_ref() + register(name)` sequence when called. Stored so that
/// the event loop can re-publish all local actors whenever a new peer is
/// discovered (Phase 1c).
pub(super) type ReRegisterFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct MeshHandle {
    peer_id: PeerId,
    /// Broadcast channel for mesh lifecycle events.
    /// Capacity 64 to absorb bursty route updates.
    pub(super) peer_events_tx: broadcast::Sender<MeshEvent>,
    /// Transport/scope-aware peer reachability table with TTL-based aging.
    routes: Arc<RouteTable>,
    /// Hostname of this node, cached at bootstrap time for display-only metadata.
    local_hostname: Arc<String>,
    /// Re-registration closures for all locally-registered actors.
    ///
    /// Populated by `register_actor`; invoked by the event loop whenever mDNS
    /// discovers a new peer so the new peer's Kademlia routing table is
    /// populated immediately rather than waiting for the next republish cycle.
    pub(super) re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
    /// The node's ed25519 identity keypair, cloned from bootstrap context.
    ///
    /// Used to sign invite grants. The keypair is the same one used by
    /// the libp2p swarm - its public key derives this node's `PeerId`.
    keypair: Arc<libp2p::identity::Keypair>,
    /// Host-side invite store for tracking issued invites.
    ///
    /// `None` when the node is a joiner (not a host). Wrapped in
    /// `Arc<RwLock<..>>` for shared access from the mesh handle and
    /// the admission handler in `RemoteNodeManager`.
    invite_store: Option<Arc<RwLock<InviteStore>>>,
    /// Unified mesh state store for joined/hosted scopes and reconnect peers.
    mesh_state_store: Option<Arc<RwLock<MeshStateStore>>>,
    /// Active transport mode used by this mesh handle.
    transport_mode: MeshTransportMode,
    /// Channel for sending commands to the swarm event loop.
    ///
    /// Used by `dial_peer()` to form a full mesh in iroh mode, where peers
    /// only connect to the inviter by default (star topology). The event
    /// loop polls the receiver and executes each `SwarmCommand`.
    swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
    /// Grace period to tolerate temporary disconnections during streaming.
    stream_reconnect_grace: std::time::Duration,
    /// Cached union of config-derived scopes (e.g. from MeshRuntimeConfig)
    /// and currently joined Iroh scopes persisted in the membership store.
    config_scopes: Arc<RwLock<Vec<MeshScopeId>>>,
}

impl std::fmt::Debug for MeshHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshHandle")
            .field("peer_id", &self.peer_id)
            .field("local_hostname", &self.local_hostname)
            .field("re_register_fns_count", &self.re_register_fns.read().len())
            .field("has_invite_store", &self.invite_store.is_some())
            .field("has_mesh_state_store", &self.mesh_state_store.is_some())
            .field("transport_mode", &self.transport_mode)
            .finish_non_exhaustive()
    }
}

impl MeshHandle {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        peer_id: PeerId,
        peer_events_tx: broadcast::Sender<MeshEvent>,
        routes: Arc<RouteTable>,
        local_hostname: String,
        re_register_fns: Arc<RwLock<HashMap<String, ReRegisterFn>>>,
        keypair: libp2p::identity::Keypair,
        invite_store: Option<Arc<RwLock<InviteStore>>>,
        mesh_state_store: Option<Arc<RwLock<MeshStateStore>>>,
        transport_mode: MeshTransportMode,
        swarm_cmd_tx: mpsc::UnboundedSender<SwarmCommand>,
        stream_reconnect_grace: std::time::Duration,
    ) -> Self {
        Self {
            peer_id,
            peer_events_tx,
            routes,
            local_hostname: Arc::new(local_hostname),
            re_register_fns,
            keypair: Arc::new(keypair),
            invite_store,
            mesh_state_store,
            transport_mode,
            swarm_cmd_tx,
            stream_reconnect_grace,
            config_scopes: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// The local `PeerId` of this node in the libp2p network.
    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    /// The hostname of this node (cached at mesh bootstrap time).
    pub fn local_hostname(&self) -> &str {
        &self.local_hostname
    }

    /// Check whether a peer is currently known to be alive (discovered and not expired).
    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.routes.is_peer_alive(peer_id)
    }

    /// Grace period used while waiting for a disconnected stream to reconnect.
    pub fn stream_reconnect_grace(&self) -> std::time::Duration {
        self.stream_reconnect_grace
    }

    /// Inject a peer directly into the `known_peers` map, bypassing mDNS.
    ///
    /// **Test-only.** In production `known_peers` is populated exclusively by
    /// mDNS `Discovered` events (which require real network time). This helper
    /// lets integration tests simulate "mDNS has fired" so that
    /// `resolve_peer_node_id` will iterate the injected peer without waiting
    /// for the actual mDNS timer.
    #[cfg(test)]
    pub fn inject_known_peer_for_test(&self, peer_id: PeerId) {
        self.routes.upsert_addrs(
            peer_id,
            crate::agent::remote::scope::MeshTransportKind::Lan,
            MeshScopeId::lan_default(),
            [libp2p::Multiaddr::empty()],
            100,
        );
    }

    /// Subscribe to peer lifecycle events (discovered / expired).
    ///
    /// Each call returns an independent receiver. Lagged receivers receive
    /// `RecvError::Lagged` and can catch up by calling `recv()` again.
    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<MeshEvent> {
        self.peer_events_tx.subscribe()
    }

    /// Register a local actor in REMOTE_REGISTRY and the Kademlia DHT.
    ///
    /// Performs the two-step sequence every mesh-visible actor needs:
    /// 1. `actor_ref.into_remote_ref().await` - inserts into `REMOTE_REGISTRY`
    ///    so incoming remote messages are routable by `ActorId`.
    /// 2. `actor_ref.register(name).await` - publishes under `name` in the
    ///    Kademlia DHT so remote peers can discover the actor by name.
    ///
    /// A warning is logged on DHT registration failure but the function does
    /// not return an error - the actor is still locally routable.
    ///
    /// Additionally stores a re-registration closure so that when a new peer
    /// is discovered via mDNS, all locally registered actors are immediately
    /// re-published into the new peer's Kademlia routing table (Phase 1c).
    pub async fn register_actor<A>(
        &self,
        actor_ref: kameo::actor::ActorRef<A>,
        name: impl Into<String>,
    ) where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let name = name.into();
        actor_ref.into_remote_ref().await;
        if let Err(e) = actor_ref.register(name.clone()).await {
            log::warn!("MeshHandle: failed to register '{}' in DHT: {}", name, e);
        } else {
            log::debug!("MeshHandle: registered '{}' in kameo DHT", name);
        }

        let name_clone = name.clone();
        let actor_ref_clone = actor_ref.clone();
        let re_register: ReRegisterFn = Arc::new(move || {
            let name = name_clone.clone();
            let actor_ref = actor_ref_clone.clone();
            Box::pin(async move {
                actor_ref.into_remote_ref().await;
                if let Err(e) = actor_ref.register(name.clone()).await {
                    log::warn!(
                        "MeshHandle: re-registration of '{}' after peer discovery failed: {}",
                        name,
                        e
                    );
                } else {
                    log::debug!(
                        "MeshHandle: re-registered '{}' after new peer discovery",
                        name
                    );
                }
            })
        });
        self.re_register_fns
            .write()
            .insert(name.clone(), re_register);
    }

    /// Look up a remote actor by its DHT name.
    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        const BACKOFF_MS: &[u64] = &[250, 500, 1_000];
        let name: String = name.into();

        if let Some(r) = kameo::actor::RemoteActorRef::<A>::lookup(name.clone()).await? {
            return Ok(Some(r));
        }

        for (attempt, &delay_ms) in BACKOFF_MS.iter().enumerate() {
            tracing::debug!(
                attempt = attempt + 1,
                delay_ms,
                dht_name = %name,
                "DHT lookup miss, retrying after backoff"
            );
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

            if let Some(r) = kameo::actor::RemoteActorRef::<A>::lookup(name.clone()).await? {
                tracing::info!(
                    attempt = attempt + 1,
                    dht_name = %name,
                    "DHT lookup succeeded on retry"
                );
                return Ok(Some(r));
            }
        }

        Ok(None)
    }

    /// Look up a single remote actor by its DHT name **without** retries.
    pub async fn lookup_actor_no_retry<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let name: String = name.into();
        kameo::actor::RemoteActorRef::<A>::lookup(name).await
    }

    /// Stream all remote actors registered under `name` in the DHT.
    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        kameo::actor::RemoteActorRef::<A>::lookup_all(name.into())
    }

    /// Resolve a human-readable peer name (from `[[mesh.peers]]`) to a `NodeId`.
    pub async fn resolve_peer_node_id(
        &self,
        peer_name: &str,
    ) -> Option<NodeId> {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};

        let peers: Vec<PeerId> = self.routes.peer_ids();

        for peer_id in peers {
            let mut node_manager = None;
            for scope in self.active_scopes() {
                let per_peer_name =
                    scoped_node_manager_for_peer(&scope, &peer_id);
                match self.lookup_actor::<RemoteNodeManager>(&per_peer_name).await {
                    Ok(Some(r)) => {
                        node_manager = Some(r);
                        break;
                    }
                    Ok(None) => {
                        log::debug!(
                            "resolve_peer_node_id: no RemoteNodeManager under '{}'",
                            per_peer_name
                        );
                    }
                    Err(e) => {
                        log::debug!(
                            "resolve_peer_node_id: lookup error for '{}': {}",
                            per_peer_name,
                            e
                        );
                    }
                }
            }
            let Some(node_manager) = node_manager else {
                continue;
            };

            let node_info = match node_manager.ask::<GetNodeInfo>(&GetNodeInfo).await {
                Ok(info) => info,
                Err(e) => {
                    log::debug!(
                        "resolve_peer_node_id: GetNodeInfo failed for peer {}: {}",
                        peer_id,
                        e
                    );
                    continue;
                }
            };

            if node_info.hostname == peer_name {
                log::info!(
                    "resolve_peer_node_id: resolved '{}' -> node_id={}",
                    peer_name,
                    node_info.node_id
                );
                return Some(node_info.node_id);
            }
        }

        log::debug!(
            "resolve_peer_node_id: no peer with hostname '{}' found in {} known peers",
            peer_name,
            self.routes.peer_count()
        );
        None
    }

    pub fn deregister_actor(&self, name: &str) {
        self.re_register_fns.write().remove(name);
    }

    pub fn re_register_fns_count(&self) -> usize {
        self.re_register_fns.read().len()
    }

    pub fn has_re_register_fn(&self, name: &str) -> bool {
        self.re_register_fns.read().contains_key(name)
    }

    pub fn known_peer_ids(&self) -> Vec<PeerId> {
        self.routes.peer_ids()
    }

    pub fn set_config_scopes(&mut self, scopes: Vec<MeshScopeId>) {
        let mut guard = self.config_scopes.write();
        *guard = scopes;
        guard.sort_by_key(|s| s.to_string());
        guard.dedup();
    }

    pub fn ensure_scope(&self, scope: MeshScopeId) -> bool {
        let mut guard = self.config_scopes.write();
        if guard.contains(&scope) {
            return false;
        }
        guard.push(scope);
        guard.sort_by_key(|s| s.to_string());
        guard.dedup();
        true
    }

    pub fn join_iroh_scope(&self, mesh_id: &str, peers: Vec<PeerId>) {
        self.ensure_scope(MeshScopeId::Iroh {
            mesh_id: mesh_id.to_string(),
        });
        if self
            .swarm_cmd_tx
            .send(SwarmCommand::JoinIrohScope {
                mesh_id: mesh_id.to_string(),
                peers,
            })
            .is_err()
        {
            log::warn!("join_iroh_scope: swarm event loop has shut down");
        }
    }

    pub(crate) fn emit_scope_joined(&self, scope: MeshScopeId) {
        let _ = self.peer_events_tx.send(MeshEvent::ScopeJoined(scope));
    }

    pub fn active_scopes(&self) -> Vec<MeshScopeId> {
        let mut scopes = Vec::new();
        scopes.extend(self.config_scopes.read().iter().cloned());

        if let Some(store) = &self.mesh_state_store {
            for mesh_id in store.read().active_mesh_ids() {
                let scope = MeshScopeId::Iroh { mesh_id };
                if !scopes.contains(&scope) {
                    scopes.push(scope);
                }
            }
        }

        if self.transport_mode.has_lan() {
            let lan = MeshScopeId::lan_default();
            if !scopes.contains(&lan) {
                scopes.push(lan);
            }
        }

        if scopes.is_empty() && self.transport_mode.has_lan() {
            scopes.push(MeshScopeId::lan_default());
        }

        scopes.sort_by_key(|s| s.to_string());
        scopes.dedup();
        scopes
    }

    pub fn joined_iroh_scopes(&self) -> Vec<MeshScopeId> {
        let Some(store) = &self.mesh_state_store else {
            return Vec::new();
        };
        store
            .read()
            .active_mesh_ids()
            .into_iter()
            .map(|mesh_id| MeshScopeId::Iroh { mesh_id })
            .collect()
    }

    pub fn leave_iroh_scope(
        &self,
        mesh_id: &str,
    ) -> Result<bool, InviteError> {
        let Some(store) = &self.mesh_state_store else {
            return Ok(false);
        };
        let removed = store.write().mark_left(mesh_id)?;
        if !removed {
            return Ok(false);
        }

        let scope = MeshScopeId::Iroh {
            mesh_id: mesh_id.to_string(),
        };
        let prefix = scope.dht_prefix();
        self.re_register_fns
            .write()
            .retain(|name, _| !name.starts_with(&prefix));
        self.config_scopes
            .write()
            .retain(|existing| existing != &scope);

        let _ = self
            .peer_events_tx
            .send(MeshEvent::ScopeLeft(MeshScopeId::Iroh {
                mesh_id: mesh_id.to_string(),
            }));

        if self
            .swarm_cmd_tx
            .send(SwarmCommand::LeaveIrohScope {
                mesh_id: mesh_id.to_string(),
            })
            .is_err()
        {
            log::warn!("leave_iroh_scope: swarm event loop has shut down");
        }
        Ok(true)
    }

    pub fn best_route_for_peer(&self, peer_id: &PeerId) -> Option<MeshRoute> {
        self.routes.best_route_for_peer(peer_id)
    }

    pub fn route_peer_ids(&self) -> Vec<PeerId> {
        self.routes.peer_ids()
    }

    pub fn create_invite(
        &self,
        mesh_name: Option<String>,
        ttl_secs: Option<u64>,
        max_uses: Option<u32>,
        can_invite: bool,
    ) -> Result<SignedInviteGrant, InviteError> {
        let max_uses = max_uses.unwrap_or(1);
        let permissions = InvitePermissions {
            can_invite,
            role: "member".to_string(),
        };

        let mesh_name_for_scope = mesh_name.clone();
        let invite = if let Some(ref store) = self.invite_store {
            store.write().create_invite(
                &self.keypair,
                &self.peer_id.to_string(),
                mesh_name,
                ttl_secs,
                max_uses,
                permissions,
            )
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let expires_at = ttl_secs.map(|ttl| now + ttl).unwrap_or(0);

            let grant = InviteGrant {
                version: 3,
                invite_id: uuid::Uuid::now_v7().to_string(),
                inviter_peer_id: self.peer_id.to_string(),
                mesh_name,
                expires_at,
                max_uses,
                permissions,
            };
            grant.sign(&self.keypair)
        }?;

        let mesh_id = mesh_id_for(
            &self.peer_id.to_string(),
            mesh_name_for_scope.as_deref(),
        );
        self.ensure_scope(MeshScopeId::Iroh {
            mesh_id: mesh_id.clone(),
        });

        if let Some(ref store) = self.mesh_state_store
            && let Err(e) = store.write().upsert_hosted_mesh(
                mesh_id.clone(),
                mesh_name_for_scope.clone(),
                Some(invite.grant.invite_id.clone()),
            )
        {
            log::warn!("Failed to persist hosted mesh state for {}: {}", mesh_id, e);
        }

        Ok(invite)
    }

    pub fn keypair(&self) -> &libp2p::identity::Keypair {
        &self.keypair
    }

    pub fn invite_store(&self) -> Option<&Arc<RwLock<InviteStore>>> {
        self.invite_store.as_ref()
    }

    pub fn mesh_state_store(
        &self,
    ) -> Option<&Arc<RwLock<MeshStateStore>>> {
        self.mesh_state_store.as_ref()
    }

    pub fn update_mesh_state_peers(
        &self,
        mesh_id: &str,
        peers: Vec<PeerEntry>,
    ) {
        if let Some(ref store) = self.mesh_state_store
            && let Err(e) = store.write().update_known_peers(mesh_id, peers)
        {
            log::warn!(
                "Failed to update mesh-state peer cache for {}: {}",
                mesh_id,
                e
            );
        }
    }

    #[deprecated(
        since = "0.1.0",
        note = "use MeshRuntimeHandle::has_transport() or MeshRuntimeHandle::enabled_transports()"
    )]
    pub fn transport_mode(&self) -> MeshTransportMode {
        self.transport_mode.clone()
    }

    pub(crate) fn raw_transport_mode(&self) -> MeshTransportMode {
        self.transport_mode.clone()
    }

    #[deprecated(
        since = "0.1.0",
        note = "use MeshRuntimeHandle::has_transport(MeshTransportKind::Iroh)"
    )]
    pub fn is_iroh_transport(&self) -> bool {
        matches!(self.transport_mode, MeshTransportMode::Iroh)
    }

    pub fn dial_peer(&self, peer_id: &PeerId) {
        self.dial_peer_with_scope(peer_id, None, DialReason::Manual);
    }

    pub(crate) fn dial_peer_for_admission(&self, peer_id: &PeerId, scope: MeshScopeId) {
        self.dial_peer_with_scope(peer_id, Some(scope), DialReason::Admission);
    }

    pub(crate) fn dial_existing_iroh_peer(&self, peer_id: &PeerId, scope: MeshScopeId) {
        self.dial_peer_with_scope(peer_id, Some(scope), DialReason::ExistingMeshPeer);
    }

    fn dial_peer_with_scope(
        &self,
        peer_id: &PeerId,
        scope: Option<MeshScopeId>,
        reason: DialReason,
    ) {
        if peer_id == &self.peer_id {
            return;
        }
        if matches!(self.transport_mode, MeshTransportMode::Lan) {
            log::debug!("dial_peer ignored on LAN-only transport (mDNS handles discovery)");
            return;
        }

        if self
            .swarm_cmd_tx
            .send(SwarmCommand::DialPeer {
                peer_id: *peer_id,
                scope,
                reason,
            })
            .is_err()
        {
            log::warn!("dial_peer: swarm event loop has shut down");
        }
    }

    pub(crate) fn is_iroh_transport_internal(&self) -> bool {
        matches!(
            self.transport_mode,
            MeshTransportMode::Iroh | MeshTransportMode::Composite
        )
    }
}
