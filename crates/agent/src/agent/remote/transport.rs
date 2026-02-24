//! `MeshTransport` trait and `DynMeshTransport` wrapper.
//!
//! ## Why this exists
//!
//! The concrete [`MeshHandle`] type couples every call site directly to the
//! libp2p/kameo implementation.  This module introduces a thin abstraction so
//! that:
//!
//! * Tests can swap in a mock transport without a live libp2p swarm.
//! * `CachedMeshTransport` (Phase 3) can be dropped in transparently.
//! * The rest of the crate never needs to change when the transport changes.
//!
//! ## Object-safety caveat
//!
//! The generic methods `register_actor` / `lookup_actor` / `lookup_all_actors`
//! make the trait **not object-safe**.  We follow **Option B** from the plan:
//! a `DynMeshTransport` wrapper provides concrete, monomorphised methods for
//! every actor type used remotely, delegating to the inner `MeshHandle`.
//! The trait itself is retained for documentation, testing, and future use.

use kameo::actor::{ActorRef, RemoteActorRef};
use kameo::remote::LookupStream;
use libp2p::PeerId;
use std::sync::Arc;
use tokio::sync::broadcast;

use super::mesh::{MeshHandle, PeerEvent};

// ── Re-export actor types used remotely ──────────────────────────────────────

use super::event_relay::EventRelayActor;
use super::node_manager::RemoteNodeManager;
use super::provider_host::{ProviderHostActor, StreamReceiverActor};
use crate::agent::session_actor::SessionActor;

// ── MeshTransport trait ───────────────────────────────────────────────────────

/// Abstraction over the mesh networking layer.
///
/// [`MeshHandle`] implements this trait directly. Future implementations may
/// add caching, different discovery mechanisms, or alternative transports while
/// keeping the rest of the crate unchanged.
///
/// **Note on object safety:** the generic methods (`register_actor`,
/// `lookup_actor`, `lookup_all_actors`) make this trait *not object-safe*.
/// Production code uses [`DynMeshTransport`], which provides concrete
/// per-actor-type methods. The trait is kept for documentation, unit-test
/// mocking (with a concrete type parameter), and future use.
#[async_trait::async_trait]
pub trait MeshTransport: Send + Sync + 'static {
    /// The local peer identity of this node.
    fn peer_id(&self) -> &PeerId;

    /// Human-readable hostname for display (cached at bootstrap).
    fn local_hostname(&self) -> &str;

    /// Whether a peer is currently considered alive (mDNS not expired).
    fn is_peer_alive(&self, peer_id: &PeerId) -> bool;

    /// Subscribe to peer lifecycle events.
    fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent>;

    /// Register a local actor in the DHT under `name`.
    async fn register_actor<A>(&self, actor_ref: ActorRef<A>, name: String)
    where
        A: kameo::Actor + kameo::remote::RemoteActor;

    /// Look up a single remote actor by DHT name (with implementation-defined
    /// retry/caching policy).
    async fn lookup_actor<A>(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor;

    /// Stream all remote actors registered under `name`.
    fn lookup_all_actors<A>(&self, name: &str) -> LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor;
}

// ── MeshTransport impl for MeshHandle ────────────────────────────────────────

#[async_trait::async_trait]
impl MeshTransport for MeshHandle {
    fn peer_id(&self) -> &PeerId {
        self.peer_id()
    }

    fn local_hostname(&self) -> &str {
        self.local_hostname()
    }

    fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.is_peer_alive(peer_id)
    }

    fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.subscribe_peer_events()
    }

    async fn register_actor<A>(&self, actor_ref: ActorRef<A>, name: String)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.register_actor(actor_ref, name).await
    }

    async fn lookup_actor<A>(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.lookup_actor::<A>(name).await
    }

    fn lookup_all_actors<A>(&self, name: &str) -> LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.lookup_all_actors::<A>(name)
    }
}

// ── DynMeshTransport ─────────────────────────────────────────────────────────

/// Object-safe wrapper around [`MeshHandle`] (or any future transport).
///
/// Provides one concrete method per actor type used remotely. The generic
/// trait methods are monomorphised here so call sites get concrete types
/// without propagating a generic parameter through the entire codebase.
///
/// This is a thin `Arc<MeshHandle>` newtype — cloning is cheap.
#[derive(Clone, Debug)]
pub struct DynMeshTransport {
    inner: Arc<MeshHandle>,
}

impl DynMeshTransport {
    /// Wrap a [`MeshHandle`] in a `DynMeshTransport`.
    pub fn new(handle: MeshHandle) -> Self {
        Self {
            inner: Arc::new(handle),
        }
    }

    /// The underlying [`MeshHandle`].
    pub fn handle(&self) -> &MeshHandle {
        &self.inner
    }

    // ── Forwarded non-generic methods ─────────────────────────────────────────

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

    /// Subscribe to peer lifecycle events.
    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.inner.subscribe_peer_events()
    }

    // ── Concrete register/lookup methods per actor type ───────────────────────

    /// Register any actor by name (generic, delegates to `MeshHandle`).
    pub async fn register_actor<A>(
        &self,
        actor_ref: kameo::actor::ActorRef<A>,
        name: impl Into<String>,
    ) where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.register_actor(actor_ref, name).await
    }

    /// Look up a [`ProviderHostActor`] by DHT name.
    pub async fn lookup_provider_host(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<ProviderHostActor>>, kameo::error::RegistryError> {
        self.inner.lookup_actor::<ProviderHostActor>(name).await
    }

    /// Look up a [`RemoteNodeManager`] by DHT name.
    pub async fn lookup_node_manager(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<RemoteNodeManager>>, kameo::error::RegistryError> {
        self.inner.lookup_actor::<RemoteNodeManager>(name).await
    }

    /// Look up a [`SessionActor`] by DHT name.
    pub async fn lookup_session(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<SessionActor>>, kameo::error::RegistryError> {
        self.inner.lookup_actor::<SessionActor>(name).await
    }

    /// Look up an [`EventRelayActor`] by DHT name.
    pub async fn lookup_event_relay(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<EventRelayActor>>, kameo::error::RegistryError> {
        self.inner.lookup_actor::<EventRelayActor>(name).await
    }

    /// Register a [`StreamReceiverActor`] in the DHT.
    pub async fn register_stream_receiver(
        &self,
        actor_ref: kameo::actor::ActorRef<StreamReceiverActor>,
        name: impl Into<String>,
    ) {
        self.inner.register_actor(actor_ref, name).await
    }

    /// Stream all [`RemoteNodeManager`] actors registered under `name`.
    pub fn lookup_all_node_managers(&self, name: &str) -> LookupStream<RemoteNodeManager> {
        self.inner.lookup_all_actors::<RemoteNodeManager>(name)
    }

    /// Generic lookup — useful when the actor type is known at the call site
    /// and no concrete wrapper exists yet.
    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_actor::<A>(name.into()).await
    }

    /// Generic lookup-all — delegates to inner `MeshHandle`.
    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.inner.lookup_all_actors::<A>(name.into())
    }
}

// Allow converting a `MeshHandle` directly into a `DynMeshTransport`.
impl From<MeshHandle> for DynMeshTransport {
    fn from(handle: MeshHandle) -> Self {
        Self::new(handle)
    }
}
