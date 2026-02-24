//! `CachedMeshTransport` — a `DynMeshTransport` wrapper that maintains a
//! local peer→actor cache for near-instant lookups.
//!
//! ## How it works
//!
//! 1. **`register_actor`:** delegates to the inner `MeshHandle` *and* stores
//!    the name in a shared `RegistrationEntry` list so `RegistryExchangeActor`
//!    can serve it to peers on demand.
//!
//! 2. **On peer discovery (mDNS `Discovered`):** looks up the new peer's
//!    `RegistryExchangeActor`, calls `GetRegistrations`, and for each entry
//!    pre-warms the cache with a Kademlia lookup.
//!
//! 3. **`lookup_actor` (via `DynMeshTransport` concrete methods):** checks the
//!    local `HashMap<String, CachedEntry>` first.  On hit, returns the cloned
//!    `RemoteActorRef` (zero network calls).  On miss, falls back to the inner
//!    `MeshHandle` (which already has Phase 1b retry built-in) and caches the
//!    result.
//!
//! 4. **On peer expiry (mDNS `Expired`):** evicts all cache entries for that
//!    peer (determined by `RemoteActorRef::id().peer_id()`).
//!
//! ## Cache representation
//!
//! Because `RemoteActorRef::new()` is `pub(crate)` in kameo we cannot
//! reconstruct refs from raw IDs. Instead we store the `RemoteActorRef`
//! itself (it implements `Clone`) boxed as `Arc<dyn Any + Send + Sync>` and
//! downcast on lookup. This is safe because each entry's actor type is fixed
//! at insertion time.
//!
//! ## `DirectoryMode` integration
//!
//! `bootstrap_mesh()` returns a `DynMeshTransport`; when
//! `MeshConfig::directory == DirectoryMode::Cached` it wraps the result in a
//! `CachedMeshTransport` first.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use kameo::actor::{ActorRef, RemoteActorRef, Spawn};
use libp2p::PeerId;
use tokio::sync::broadcast;
use tracing::Instrument;

use super::mesh::{MeshHandle, PeerEvent};
use super::registry_exchange::{GetRegistrations, RegistrationEntry, RegistryExchangeActor};

// ── Cache entry ───────────────────────────────────────────────────────────────

struct CachedEntry {
    peer_id: Option<PeerId>,
    /// Type-erased `RemoteActorRef<A>` stored as `Any`. Downcast on lookup.
    ref_any: Arc<dyn Any + Send + Sync>,
    #[allow(dead_code)]
    cached_at: Instant,
}

// ── CachedMeshTransport ───────────────────────────────────────────────────────

/// A `DynMeshTransport` that maintains a local actor cache.
///
/// Constructed via [`CachedMeshTransport::new`] and converted into a
/// `DynMeshTransport`-compatible value by calling [`into_handle`].
pub struct CachedMeshTransport {
    inner: Arc<MeshHandle>,
    /// name → list of cached refs (one per peer that registered under that name)
    cache: Arc<RwLock<HashMap<String, Vec<CachedEntry>>>>,
    /// Local registration table, shared with `RegistryExchangeActor`.
    local_registrations: Arc<RwLock<Vec<RegistrationEntry>>>,
}

impl CachedMeshTransport {
    /// Create a `CachedMeshTransport` wrapping `handle`.
    ///
    /// Spawns a `RegistryExchangeActor`, registers it in the DHT, and starts
    /// a background task that:
    /// * pre-warms the cache whenever mDNS discovers a new peer.
    /// * evicts stale entries when a peer's mDNS record expires.
    pub async fn new(handle: MeshHandle) -> Self {
        let local_registrations: Arc<RwLock<Vec<RegistrationEntry>>> =
            Arc::new(RwLock::new(Vec::new()));
        let cache: Arc<RwLock<HashMap<String, Vec<CachedEntry>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn and register the RegistryExchangeActor.
        let exchange_actor = RegistryExchangeActor::new(Arc::clone(&local_registrations));
        let exchange_ref = RegistryExchangeActor::spawn(exchange_actor);
        let exchange_dht_name = RegistryExchangeActor::dht_name(handle.peer_id());
        {
            let reg_span = tracing::info_span!(
                "remote.cached_transport.register_exchange",
                dht_name = %exchange_dht_name
            );
            handle
                .register_actor(exchange_ref, exchange_dht_name.clone())
                .instrument(reg_span)
                .await;
        }
        log::info!(
            "CachedMeshTransport: RegistryExchangeActor registered as '{}'",
            exchange_dht_name
        );

        let transport = Self {
            inner: Arc::new(handle),
            cache: Arc::clone(&cache),
            local_registrations,
        };

        // Start background task for cache maintenance.
        transport.spawn_cache_maintenance_task();

        transport
    }

    // ── Cache helpers ─────────────────────────────────────────────────────────

    /// Insert a typed `RemoteActorRef<A>` into the cache under `name`.
    fn insert<A: kameo::Actor + 'static + Send + Sync>(
        &self,
        name: &str,
        peer_id: Option<PeerId>,
        r: RemoteActorRef<A>,
    ) {
        let entry = CachedEntry {
            peer_id,
            ref_any: Arc::new(r) as Arc<dyn Any + Send + Sync>,
            cached_at: Instant::now(),
        };
        if let Ok(mut cache) = self.cache.write() {
            cache.entry(name.to_string()).or_default().push(entry);
        }
    }

    /// Try to retrieve a typed `RemoteActorRef<A>` from the cache.
    ///
    /// `RemoteActorRef<A>` always implements `Clone` (independent of whether
    /// `A` is `Clone`), so no extra `Clone` bound on `A` is needed.
    fn get<A: kameo::Actor + 'static + Send + Sync>(
        &self,
        name: &str,
    ) -> Option<RemoteActorRef<A>> {
        let cache = self.cache.read().ok()?;
        let entries = cache.get(name)?;
        // Return the first entry that downcasts to the right type.
        for e in entries {
            if let Some(r) = e.ref_any.downcast_ref::<RemoteActorRef<A>>() {
                return Some(r.clone());
            }
        }
        None
    }

    /// Evict all cache entries belonging to `peer_id`.
    #[allow(dead_code)]
    fn evict_peer(&self, peer_id: &PeerId) {
        if let Ok(mut cache) = self.cache.write() {
            for entries in cache.values_mut() {
                entries.retain(|e| e.peer_id.as_ref() != Some(peer_id));
            }
            // Remove empty buckets.
            cache.retain(|_, v| !v.is_empty());
        }
    }

    // ── Generic lookup with caching ───────────────────────────────────────────

    /// Look up an actor by DHT name, checking the cache first.
    pub async fn lookup_cached<A>(
        &self,
        name: &str,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor + 'static + Send + Sync,
    {
        // Cache hit — zero network calls.
        if let Some(r) = self.get::<A>(name) {
            tracing::debug!(dht_name = %name, "cache hit");
            return Ok(Some(r));
        }

        // Cache miss — fall back to Kademlia (with Phase 1b retry).
        tracing::debug!(dht_name = %name, "cache miss, falling back to Kademlia");
        let result = self.inner.lookup_actor::<A>(name).await?;
        if let Some(ref r) = result {
            let peer_id = r.id().peer_id().copied();
            self.insert::<A>(name, peer_id, r.clone());
            tracing::debug!(dht_name = %name, "cached result from Kademlia lookup");
        }
        Ok(result)
    }

    // ── Background maintenance task ───────────────────────────────────────────

    fn spawn_cache_maintenance_task(&self) {
        let mut rx = self.inner.subscribe_peer_events();
        let inner = Arc::clone(&self.inner);
        let cache = Arc::clone(&self.cache);

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(PeerEvent::Discovered(peer_id)) => {
                        let inner2 = Arc::clone(&inner);
                        let cache2 = Arc::clone(&cache);
                        tokio::spawn(async move {
                            prewarm_cache_for_peer(&inner2, &cache2, peer_id).await;
                        });
                    }
                    Ok(PeerEvent::Expired(peer_id)) => {
                        // Evict all cache entries for this peer.
                        if let Ok(mut c) = cache.write() {
                            for entries in c.values_mut() {
                                entries.retain(|e| e.peer_id.as_ref() != Some(&peer_id));
                            }
                            c.retain(|_, v| !v.is_empty());
                        }
                        log::debug!(
                            "CachedMeshTransport: evicted cache entries for expired peer {}",
                            peer_id
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // ── Expose the inner MeshHandle ────────────────────────────────────────────

    /// The underlying [`MeshHandle`].
    pub fn handle(&self) -> &MeshHandle {
        &self.inner
    }
}

/// Pre-warm the cache for a newly discovered peer.
///
/// 1. Looks up the peer's `RegistryExchangeActor` in the DHT.
/// 2. Calls `GetRegistrations` to retrieve all actor names registered there.
/// 3. For each name, does a Kademlia lookup (Phase 1b retry included) and
///    stores the resulting `RemoteActorRef` in `cache`.
///
/// Only `ProviderHostActor` and `RemoteNodeManager` are pre-warmed here
/// because those are the actors that suffer most from cold-lookup latency.
/// Extending to more types is straightforward but increases startup chatter.
async fn prewarm_cache_for_peer(
    handle: &MeshHandle,
    cache: &Arc<RwLock<HashMap<String, Vec<CachedEntry>>>>,
    peer_id: PeerId,
) {
    use super::node_manager::RemoteNodeManager;
    use super::provider_host::ProviderHostActor;

    let exchange_name = RegistryExchangeActor::dht_name(&peer_id);
    let exchange_ref = match handle
        .lookup_actor::<RegistryExchangeActor>(&exchange_name)
        .await
    {
        Ok(Some(r)) => r,
        Ok(None) => {
            log::debug!(
                "CachedMeshTransport: no RegistryExchangeActor on peer {} (old node?)",
                peer_id
            );
            return;
        }
        Err(e) => {
            log::debug!(
                "CachedMeshTransport: RegistryExchangeActor lookup failed for {}: {}",
                peer_id,
                e
            );
            return;
        }
    };

    let registrations: Vec<RegistrationEntry> = match exchange_ref
        .ask::<GetRegistrations>(&GetRegistrations)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            log::debug!(
                "CachedMeshTransport: GetRegistrations failed for {}: {}",
                peer_id,
                e
            );
            return;
        }
    };

    log::info!(
        "CachedMeshTransport: pre-warming cache for peer {} ({} entries)",
        peer_id,
        registrations.len()
    );

    for entry in &registrations {
        let name = &entry.dht_name;

        // Detect actor type from the DHT name prefix and pre-warm accordingly.
        if name.starts_with("provider_host::") {
            match handle.lookup_actor::<ProviderHostActor>(name).await {
                Ok(Some(r)) => {
                    insert_into_cache(cache, name, Some(peer_id), r);
                    log::debug!(
                        "CachedMeshTransport: pre-warmed ProviderHostActor '{}'",
                        name
                    );
                }
                Ok(None) => {
                    log::debug!(
                        "CachedMeshTransport: ProviderHostActor '{}' not found in DHT",
                        name
                    );
                }
                Err(e) => {
                    log::debug!(
                        "CachedMeshTransport: ProviderHostActor '{}' lookup error: {}",
                        name,
                        e
                    );
                }
            }
        } else if name.starts_with("node_manager") {
            match handle.lookup_actor::<RemoteNodeManager>(name).await {
                Ok(Some(r)) => {
                    insert_into_cache(cache, name, Some(peer_id), r);
                    log::debug!(
                        "CachedMeshTransport: pre-warmed RemoteNodeManager '{}'",
                        name
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!(
                        "CachedMeshTransport: RemoteNodeManager '{}' lookup error: {}",
                        name,
                        e
                    );
                }
            }
        }
        // Other actor types (session, stream_rx, event_relay) are ephemeral
        // or session-scoped — not worth pre-warming.
    }
}

/// Helper to insert a `RemoteActorRef<A>` into the cache map.
fn insert_into_cache<A: kameo::Actor + kameo::remote::RemoteActor + 'static + Send + Sync>(
    cache: &Arc<RwLock<HashMap<String, Vec<CachedEntry>>>>,
    name: &str,
    peer_id: Option<PeerId>,
    r: RemoteActorRef<A>,
) {
    let entry = CachedEntry {
        peer_id,
        ref_any: Arc::new(r) as Arc<dyn Any + Send + Sync>,
        cached_at: Instant::now(),
    };
    if let Ok(mut c) = cache.write() {
        c.entry(name.to_string()).or_default().push(entry);
    }
}

// ── CachedDynMeshTransport — public façade ────────────────────────────────────

/// Public wrapper that exposes the same interface as [`DynMeshTransport`] but
/// with an in-process cache in front of Kademlia.
///
/// Construct via [`CachedDynMeshTransport::new`], then use wherever a
/// `DynMeshTransport` is expected — both types expose the same set of methods.
pub struct CachedDynMeshTransport {
    cached: Arc<CachedMeshTransport>,
}

impl CachedDynMeshTransport {
    /// Wrap a [`MeshHandle`] with a pre-warming cache layer.
    pub async fn new(handle: MeshHandle) -> Self {
        Self {
            cached: Arc::new(CachedMeshTransport::new(handle).await),
        }
    }

    // ── Forwarded non-generic methods (same as DynMeshTransport) ─────────────

    pub fn peer_id(&self) -> &PeerId {
        self.cached.inner.peer_id()
    }

    pub fn local_hostname(&self) -> &str {
        self.cached.inner.local_hostname()
    }

    pub fn is_peer_alive(&self, peer_id: &PeerId) -> bool {
        self.cached.inner.is_peer_alive(peer_id)
    }

    pub fn subscribe_peer_events(&self) -> broadcast::Receiver<PeerEvent> {
        self.cached.inner.subscribe_peer_events()
    }

    pub async fn register_actor<A>(&self, actor_ref: ActorRef<A>, name: impl Into<String>)
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        let name = name.into();
        // Record in local registration table for RegistryExchangeActor.
        let actor_seq_id = actor_ref.id().sequence_id();
        let entry = RegistrationEntry {
            dht_name: name.clone(),
            actor_sequence_id: actor_seq_id,
        };
        if let Ok(mut reg) = self.cached.local_registrations.write()
            && !reg.iter().any(|e| e.dht_name == name)
        {
            reg.push(entry);
        }
        self.cached.inner.register_actor(actor_ref, name).await
    }

    /// Look up a [`ProviderHostActor`] — cache-first.
    pub async fn lookup_provider_host(
        &self,
        name: &str,
    ) -> Result<
        Option<RemoteActorRef<super::provider_host::ProviderHostActor>>,
        kameo::error::RegistryError,
    > {
        self.cached
            .lookup_cached::<super::provider_host::ProviderHostActor>(name)
            .await
    }

    /// Look up a [`RemoteNodeManager`] — cache-first.
    pub async fn lookup_node_manager(
        &self,
        name: &str,
    ) -> Result<
        Option<RemoteActorRef<super::node_manager::RemoteNodeManager>>,
        kameo::error::RegistryError,
    > {
        self.cached
            .lookup_cached::<super::node_manager::RemoteNodeManager>(name)
            .await
    }

    /// Look up a [`SessionActor`] — cache-first.
    pub async fn lookup_session(
        &self,
        name: &str,
    ) -> Result<
        Option<RemoteActorRef<crate::agent::session_actor::SessionActor>>,
        kameo::error::RegistryError,
    > {
        self.cached
            .lookup_cached::<crate::agent::session_actor::SessionActor>(name)
            .await
    }

    /// Generic lookup — cache-first.
    pub async fn lookup_actor<A>(
        &self,
        name: impl Into<String>,
    ) -> Result<Option<RemoteActorRef<A>>, kameo::error::RegistryError>
    where
        A: kameo::Actor + kameo::remote::RemoteActor + 'static + Send + Sync,
    {
        self.cached.lookup_cached::<A>(&name.into()).await
    }

    /// Generic lookup-all — delegates directly to inner (no cache).
    pub fn lookup_all_actors<A>(&self, name: impl Into<String>) -> kameo::remote::LookupStream<A>
    where
        A: kameo::Actor + kameo::remote::RemoteActor,
    {
        self.cached.inner.lookup_all_actors::<A>(name.into())
    }

    /// The underlying [`MeshHandle`].
    pub fn handle(&self) -> &MeshHandle {
        self.cached.handle()
    }
}

impl Clone for CachedDynMeshTransport {
    fn clone(&self) -> Self {
        Self {
            cached: Arc::clone(&self.cached),
        }
    }
}

impl std::fmt::Debug for CachedDynMeshTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedDynMeshTransport")
            .field("peer_id", self.cached.inner.peer_id())
            .finish_non_exhaustive()
    }
}
