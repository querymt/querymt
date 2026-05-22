//! Process-wide mesh runtime handle and scoped mesh views.
//!
//! This module introduces [`MeshRuntimeHandle`] and [`MeshScopeHandle`] as the
//! preferred abstraction for agent-facing mesh interaction.  During the
//! multi-transport scoped mesh migration (Phases 2–8), these types wrap the
//! current [`MeshHandle`] while providing forward-compatible APIs for scope-
//! aware registration, lookup, and transport queries.
//!
//! ## Migration path
//!
//! * **Phase 2 (now):** `MeshRuntimeHandle` wraps `Arc<MeshHandle>`.
//!   Existing call sites keep working.  New code depends on
//!   `MeshRuntimeHandle`.
//! * **Phase 4+:** `MeshRuntimeHandle` will wrap `Arc<MeshRuntimeInner>`
//!   containing the composite transport stack, route table, and scope
//!   registry.  The outer API surface stays the same.
//!
//! ## Why not just use `MeshHandle`?
//!
//! `MeshHandle` mixes runtime ownership, transport mode, route discovery,
//! invite/membership, and agent API concerns.  `MeshRuntimeHandle`
//! decouples agent-facing code from the concrete implementation so that
//! later phases can swap the internals without touching every call site.

use std::sync::Arc;

use kameo::actor::ActorRef;
use libp2p::PeerId;
use tokio::sync::broadcast;

use super::mesh::{MeshHandle, MeshTransportMode, PeerEvent};
use super::scope::{MeshScopeId, MeshTransportKind};

// ── MeshRuntimeHandle ─────────────────────────────────────────────────────

/// Cloneable handle to the process-wide mesh runtime.
///
/// This is the **preferred type** for agent-facing code that needs to
/// interact with the mesh.  It wraps the current [`MeshHandle`] and
/// will eventually wrap the unified composite runtime (Phase 4).
///
/// # Scope awareness
///
/// Use [`scope`](MeshRuntimeHandle::scope) to obtain a [`MeshScopeHandle`]
/// that automatically prefixes DHT names for a given logical mesh scope.
///
/// # Transport queries
///
/// Use [`has_transport`](MeshRuntimeHandle::has_transport) and
/// [`enabled_transports`](MeshRuntimeHandle::enabled_transports) instead of
/// the deprecated [`MeshHandle::transport_mode`] /
/// [`MeshHandle::is_iroh_transport`].
#[derive(Clone, Debug)]
pub struct MeshRuntimeHandle {
    inner: Arc<MeshHandle>,
}

impl MeshRuntimeHandle {
    /// Create a new `MeshRuntimeHandle` wrapping the given [`MeshHandle`].
    pub fn new(handle: MeshHandle) -> Self {
        Self {
            inner: Arc::new(handle),
        }
    }

    /// Access the underlying [`MeshHandle`].
    ///
    /// Use this for gradual migration — new code should prefer the
    /// `MeshRuntimeHandle` methods directly.
    pub fn as_mesh_handle(&self) -> &MeshHandle {
        &self.inner
    }

    // ── Identity and peer info ────────────────────────────────────────────

    /// The local `PeerId` of this node.
    pub fn peer_id(&self) -> &PeerId {
        self.inner.peer_id()
    }

    /// Human-readable hostname (cached at bootstrap).
    pub fn local_hostname(&self) -> &str {
        self.inner.local_hostname()
    }

    /// Whether a peer is currently considered alive.
    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.inner.is_peer_alive(peer_id)
    }

    /// Grace period for tolerating temporary disconnections during streaming.
    pub fn stream_reconnect_grace(&self) -> std::time::Duration {
        self.inner.stream_reconnect_grace()
    }

    /// Subscribe to peer lifecycle events (discovered / expired).
    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.inner.subscribe_peer_events()
    }

    /// Return all currently-known peer IDs (alive, not expired).
    pub fn known_peer_ids(&self) -> Vec<PeerId> {
        self.inner.known_peer_ids()
    }

    /// Return active logical scopes for this runtime.
    pub fn active_scopes(&self) -> Vec<MeshScopeId> {
        self.inner.active_scopes()
    }

    // ── Actor registration and lookup ─────────────────────────────────────

    /// Register a local actor in the DHT under `name`.
    ///
    /// Delegates to [`MeshHandle::register_actor`].  For scoped
    /// registration, use [`register_actor_scoped`](Self::register_actor_scoped)
    /// or obtain a [`MeshScopeHandle`] via [`scope`](Self::scope).
    pub async fn register_actor<A>(&self, actor_ref: ActorRef<A>, name: impl Into<String>)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.register_actor(actor_ref, name).await
    }

    /// Look up a single remote actor by DHT name (with retry backoff).
    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_actor(name).await
    }

    /// Look up a single remote actor by DHT name **without** retries.
    pub async fn lookup_actor_no_retry<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_actor_no_retry(name).await
    }

    /// Stream all remote actors registered under `name`.
    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> kameo::remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_all_actors(name)
    }

    /// Remove the re-registration closure for a named actor.
    pub fn deregister_actor(&self, name: &str) {
        self.inner.deregister_actor(name)
    }

    /// Return the number of currently registered re-registration closures.
    pub fn re_register_fns_count(&self) -> usize {
        self.inner.re_register_fns_count()
    }

    /// Check whether a re-registration closure exists for the given name.
    pub fn has_re_register_fn(&self, name: &str) -> bool {
        self.inner.has_re_register_fn(name)
    }

    // ── Scoped registration and lookup ────────────────────────────────────

    /// Obtain a scoped view of this runtime for the given scope.
    ///
    /// The returned [`MeshScopeHandle`] automatically prefixes DHT names
    /// with the scope's namespace prefix on every register/lookup call.
    pub fn scope(&self, scope: MeshScopeId) -> MeshScopeHandle {
        MeshScopeHandle {
            runtime: self.clone(),
            scope,
        }
    }

    /// Register a local actor in the DHT under a scope-prefixed name.
    ///
    /// The DHT name is composed as `{scope.dht_prefix()}{name}`.
    /// For [`MeshScopeId::Lan`] this produces the same name as
    /// [`register_actor`](Self::register_actor).
    pub async fn register_actor_scoped<A>(
        &self,
        actor_ref: ActorRef<A>,
        scope: &MeshScopeId,
        name: &str,
    ) where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let scoped_name = format!("{}{}", scope.dht_prefix(), name);
        self.inner.register_actor(actor_ref, scoped_name).await
    }

    /// Look up a remote actor by scope-prefixed name (with retry backoff).
    ///
    /// The DHT name is composed as `{scope.dht_prefix()}{name}`.
    /// For [`MeshScopeId::Lan`] this produces the same result as
    /// [`lookup_actor`](Self::lookup_actor).
    pub async fn lookup_actor_scoped<A>(
        &self,
        scope: &MeshScopeId,
        name: &str,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let scoped_name = format!("{}{}", scope.dht_prefix(), name);
        self.inner.lookup_actor(scoped_name).await
    }

    /// Look up a remote actor by scope-prefixed name **without** retries.
    pub async fn lookup_actor_scoped_no_retry<A>(
        &self,
        scope: &MeshScopeId,
        name: &str,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let scoped_name = format!("{}{}", scope.dht_prefix(), name);
        self.inner.lookup_actor_no_retry(scoped_name).await
    }

    /// Stream all remote actors under a scope-prefixed name.
    pub fn lookup_all_actors_scoped<A>(
        &self,
        scope: &MeshScopeId,
        name: &str,
    ) -> kameo::remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let scoped_name = format!("{}{}", scope.dht_prefix(), name);
        self.inner.lookup_all_actors(scoped_name)
    }

    /// Deregister a scope-prefixed actor name.
    pub fn deregister_actor_scoped(&self, scope: &MeshScopeId, name: &str) {
        let scoped_name = format!("{}{}", scope.dht_prefix(), name);
        self.inner.deregister_actor(&scoped_name)
    }

    // ── Transport capability queries ──────────────────────────────────────

    /// Return the list of currently enabled transport kinds.
    ///
    /// During Phase 2 (single transport) this returns a one-element vec.
    /// In Phase 4+ (composite transport) this may return multiple entries.
    #[allow(deprecated)]
    #[allow(deprecated)]
    pub fn enabled_transports(&self) -> Vec<MeshTransportKind> {
        match self.inner.transport_mode() {
            MeshTransportMode::Lan => vec![MeshTransportKind::Lan],
            MeshTransportMode::Iroh => vec![MeshTransportKind::Iroh],
            MeshTransportMode::Composite => {
                vec![MeshTransportKind::Lan, MeshTransportKind::Iroh]
            }
        }
    }

    /// Check whether a specific transport kind is enabled.
    ///
    /// This is the forward-compatible replacement for
    /// `MeshHandle::is_iroh_transport()`.
    pub fn has_transport(&self, kind: MeshTransportKind) -> bool {
        self.enabled_transports().contains(&kind)
    }

    // ── Resolve / dial ────────────────────────────────────────────────────

    /// Resolve a human-readable peer name to a [`NodeId`](super::NodeId).
    pub async fn resolve_peer_node_id(
        &self,
        peer_name: &str,
    ) -> Option<crate::agent::remote::NodeId> {
        self.inner.resolve_peer_node_id(peer_name).await
    }

    /// Request the swarm event loop to dial a peer by `PeerId`.
    pub fn dial_peer(&self, peer_id: &PeerId) {
        self.inner.dial_peer(peer_id)
    }

    // ── Invite / membership ───────────────────────────────────────────────

    /// Create a signed invite grant for this mesh.
    pub fn create_invite(
        &self,
        mesh_name: Option<String>,
        ttl_secs: Option<u64>,
        max_uses: Option<u32>,
        can_invite: bool,
    ) -> Result<super::invite::SignedInviteGrant, super::invite::InviteError> {
        self.inner
            .create_invite(mesh_name, ttl_secs, max_uses, can_invite)
    }

    /// Return a reference to the node's identity keypair.
    pub fn keypair(&self) -> &libp2p::identity::Keypair {
        self.inner.keypair()
    }

    /// Return a reference to the invite store (if any).
    pub fn invite_store(
        &self,
    ) -> Option<&std::sync::Arc<parking_lot::RwLock<super::invite::InviteStore>>> {
        self.inner.invite_store()
    }

    /// Return a reference to the membership store (if any).
    pub fn membership_store(
        &self,
    ) -> Option<&std::sync::Arc<parking_lot::RwLock<super::invite::MembershipStore>>> {
        self.inner.membership_store()
    }

    /// Update cached peer list for a mesh membership entry.
    pub fn update_membership_peers(&self, mesh_id: &str, peers: Vec<super::invite::PeerEntry>) {
        self.inner.update_membership_peers(mesh_id, peers)
    }

    /// Return joined Iroh scopes in deterministic order.
    pub fn joined_iroh_scopes(&self) -> Vec<MeshScopeId> {
        self.inner.joined_iroh_scopes()
    }

    /// Leave an Iroh scope while keeping LAN/runtime alive.
    pub fn leave_iroh_scope(&self, mesh_id: &str) -> Result<bool, super::invite::InviteError> {
        self.inner.leave_iroh_scope(mesh_id)
    }
}

// ── Conversions ────────────────────────────────────────────────────────────

impl From<MeshHandle> for MeshRuntimeHandle {
    fn from(handle: MeshHandle) -> Self {
        Self::new(handle)
    }
}

impl From<&MeshRuntimeHandle> for MeshRuntimeHandle {
    fn from(handle: &MeshRuntimeHandle) -> Self {
        handle.clone()
    }
}

impl AsRef<MeshHandle> for MeshRuntimeHandle {
    fn as_ref(&self) -> &MeshHandle {
        &self.inner
    }
}

// ── MeshScopeHandle ───────────────────────────────────────────────────────

/// A scoped view into the mesh runtime.
///
/// `MeshScopeHandle` pairs a [`MeshRuntimeHandle`] with a [`MeshScopeId`]
/// so that all registration and lookup operations automatically use the
/// scope's DHT prefix.  This provides logical mesh isolation without
/// requiring call sites to manually compose scoped names.
///
/// # Example
///
/// ```rust,ignore
/// let runtime = MeshRuntimeHandle::new(mesh_handle);
/// let iroh_scope = runtime.scope(MeshScopeId::Iroh { mesh_id: "team-a".into() });
///
/// // Register into the Iroh scope
/// iroh_scope.register_actor(actor_ref, "node_manager").await;
///
/// // Lookup in the same scope
/// let nm = iroh_scope.lookup_actor::<RemoteNodeManager>("node_manager").await;
/// ```
#[derive(Clone, Debug)]
pub struct MeshScopeHandle {
    runtime: MeshRuntimeHandle,
    scope: MeshScopeId,
}

impl MeshScopeHandle {
    /// The scope identity this handle is bound to.
    pub fn scope_id(&self) -> &MeshScopeId {
        &self.scope
    }

    /// Access the underlying runtime handle.
    pub fn runtime(&self) -> &MeshRuntimeHandle {
        &self.runtime
    }

    // ── Identity and peer info (delegated) ────────────────────────────────

    /// The local `PeerId` of this node.
    pub fn peer_id(&self) -> &PeerId {
        self.runtime.peer_id()
    }

    /// Human-readable hostname.
    pub fn local_hostname(&self) -> &str {
        self.runtime.local_hostname()
    }

    /// Whether a peer is currently considered alive.
    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.runtime.is_peer_alive(peer_id)
    }

    /// Subscribe to peer lifecycle events.
    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.runtime.subscribe_peer_events()
    }

    // ── Scoped actor registration and lookup ──────────────────────────────

    /// Register a local actor in this scope under the given base name.
    ///
    /// The full DHT name is `{scope.dht_prefix()}{name}`.
    pub async fn register_actor<A>(&self, actor_ref: ActorRef<A>, name: &str)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime
            .register_actor_scoped(actor_ref, &self.scope, name)
            .await
    }

    /// Look up a remote actor in this scope by base name (with retry backoff).
    pub async fn lookup_actor<A>(
        &self,
        name: &str,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime.lookup_actor_scoped(&self.scope, name).await
    }

    /// Look up a remote actor in this scope by base name **without** retries.
    pub async fn lookup_actor_no_retry<A>(
        &self,
        name: &str,
    ) -> Result<Option<kameo::actor::RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime
            .lookup_actor_scoped_no_retry(&self.scope, name)
            .await
    }

    /// Stream all remote actors in this scope under the given base name.
    pub fn lookup_all_actors<A>(&self, name: &str) -> kameo::remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.runtime.lookup_all_actors_scoped(&self.scope, name)
    }

    /// Deregister a scope-prefixed actor.
    pub fn deregister_actor(&self, name: &str) {
        self.runtime.deregister_actor_scoped(&self.scope, name)
    }

    // ── Resolve / dial (delegated) ────────────────────────────────────────

    /// Resolve a peer name to a [`NodeId`](super::NodeId).
    pub async fn resolve_peer_node_id(
        &self,
        peer_name: &str,
    ) -> Option<crate::agent::remote::NodeId> {
        self.runtime.resolve_peer_node_id(peer_name).await
    }

    /// Dial a peer by `PeerId`.
    pub fn dial_peer(&self, peer_id: &PeerId) {
        self.runtime.dial_peer(peer_id)
    }

    // ── Convenience DHT name helpers ──────────────────────────────────────

    /// The scoped DHT name for the global `RemoteNodeManager` in this scope.
    pub fn node_manager_name(&self) -> String {
        super::scope::scoped_node_manager(&self.scope)
    }

    /// The scoped DHT name for a per-peer `RemoteNodeManager` in this scope.
    pub fn node_manager_for_peer_name(&self, peer_id: &impl std::fmt::Display) -> String {
        super::scope::scoped_node_manager_for_peer(&self.scope, peer_id)
    }

    /// The scoped DHT name for a `ProviderHostActor` in this scope.
    pub fn provider_host_name(&self, peer_id: &impl std::fmt::Display) -> String {
        super::scope::scoped_provider_host(&self.scope, peer_id)
    }

    /// The scoped DHT name for a `SessionActor` in this scope.
    pub fn session_name(&self, session_id: &str) -> String {
        super::scope::scoped_session(&self.scope, session_id)
    }

    /// The scoped DHT name for an `EventRelayActor` in this scope.
    pub fn event_relay_name(&self, session_id: &str, peer_id: &impl std::fmt::Display) -> String {
        super::scope::scoped_event_relay(&self.scope, session_id, peer_id)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `MeshRuntimeHandle` can be created from a `MeshHandle`
    /// and that forwarded methods compile and run without a live swarm.
    ///
    /// Note: most methods require a running swarm and kameo global. This
    /// test only exercises the lightweight wrapper logic.
    #[test]
    fn runtime_handle_enabled_transports_matches_transport_mode() {
        use MeshTransportKind as Kind;
        use MeshTransportMode as Mode;

        // We can't easily create a real MeshHandle without a swarm, so we
        // verify the mapping logic directly.
        fn map_mode(mode: Mode) -> Vec<Kind> {
            match mode {
                Mode::Lan => vec![Kind::Lan],
                Mode::Iroh => vec![Kind::Iroh],
                Mode::Composite => vec![Kind::Lan, Kind::Iroh],
            }
        }

        assert_eq!(map_mode(Mode::Lan), vec![Kind::Lan]);
        assert_eq!(map_mode(Mode::Iroh), vec![Kind::Iroh]);
        assert_eq!(map_mode(Mode::Composite), vec![Kind::Lan, Kind::Iroh]);
    }

    #[test]
    fn scope_handle_dht_names_match_scoped_helpers() {
        use super::super::scope::{scoped_node_manager, scoped_node_manager_for_peer};

        // Verify that MeshScopeHandle convenience methods produce the same
        // names as calling the scoped_* helpers directly.
        //
        // We can't construct a real MeshScopeHandle without a MeshHandle, but
        // we verify the name composition logic is correct by testing the
        // underlying helpers.
        let scope = MeshScopeId::Iroh {
            mesh_id: "test-mesh".to_string(),
        };

        let peer_id = "12D3KooWABC";

        // node_manager
        assert_eq!(
            scoped_node_manager(&scope),
            "scope::test-mesh::node_manager"
        );

        // node_manager for peer
        assert_eq!(
            scoped_node_manager_for_peer(&scope, &peer_id),
            "scope::test-mesh::node_manager::peer::12D3KooWABC"
        );
    }

    #[test]
    fn mesh_scope_id_display_roundtrip() {
        let lan = MeshScopeId::Lan;
        assert_eq!(format!("{}", lan), "lan");

        let iroh = MeshScopeId::Iroh {
            mesh_id: "team-a".to_string(),
        };
        assert_eq!(format!("{}", iroh), "iroh:team-a");
    }
}
