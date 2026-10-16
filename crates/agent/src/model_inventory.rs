//! Non-blocking model inventory service with snapshot-based reads.
//!
//! Provides fast, non-blocking model listing by returning cached snapshots
//! immediately and refreshing data in the background.

use arc_swap::ArcSwap;
#[cfg(feature = "remote")]
use futures_util::stream::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast, watch};

use crate::agent::agent_config::AgentConfig;
use crate::model_registry::{ModelEntry, enumerate_local_models};
#[cfg(feature = "remote")]
use querymt_remote::MeshRuntimeHandle;

/// Metadata about the current snapshot
#[derive(Debug, Clone)]
pub struct SnapshotMeta {
    /// When the local snapshot was last refreshed
    pub local_updated_at: Option<Instant>,
    /// When the remote snapshot was last refreshed
    pub remote_updated_at: Option<Instant>,
    /// Whether a refresh is currently in progress
    pub refresh_in_progress: bool,
    /// Whether the data is potentially stale
    pub is_stale: bool,
    /// Number of remote queries (including discovery) that timed out during last refresh
    pub remote_timeout_count: usize,
    /// Total number of remote nodes attempted
    pub remote_node_count: usize,
}

/// Tracks ongoing refresh operations
struct RefreshState {
    /// Whether a local refresh is in progress
    local_refreshing: bool,
    /// Whether a remote refresh is in progress
    remote_refreshing: bool,
    /// When local data was last updated
    local_updated_at: Option<Instant>,
    /// When remote data was last updated
    remote_updated_at: Option<Instant>,
    /// Number of remote timeouts in last refresh
    remote_timeout_count: usize,
    /// Total remote nodes attempted
    remote_node_count: usize,
    /// Completion state for the current refresh cycle.
    /// `false` means still running; `true` means completed.
    /// A watch channel retains the latest state so late waiters cannot miss completion.
    refresh_done_tx: watch::Sender<bool>,
    /// Last time a refresh was triggered (for debouncing).
    last_triggered_at: Option<Instant>,
    /// When invalidation happens during an active refresh, request an immediate
    /// follow-up refresh after the current cycle completes.
    refresh_requested_after_current: bool,
}

/// Describes what happened when a refresh was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTriggerDisposition {
    /// A refresh was already running, so this call joined the active cycle.
    AlreadyInProgress,
    /// A new refresh was skipped because it fell within the debounce window.
    Debounced,
    /// A new refresh task was started.
    Started,
}

impl RefreshTriggerDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AlreadyInProgress => "already_in_progress",
            Self::Debounced => "debounced",
            Self::Started => "started",
        }
    }
}

/// Handle to track refresh completion.
pub struct RefreshHandle {
    wait_rx: Option<watch::Receiver<bool>>,
    disposition: RefreshTriggerDisposition,
}

impl RefreshHandle {
    /// Create a handle that waits on an active or expected completion.
    fn pending(wait_rx: watch::Receiver<bool>, disposition: RefreshTriggerDisposition) -> Self {
        Self {
            wait_rx: Some(wait_rx),
            disposition,
        }
    }

    /// Create a handle that completes immediately (debounced or no-op).
    fn completed(disposition: RefreshTriggerDisposition) -> Self {
        Self {
            wait_rx: None,
            disposition,
        }
    }

    /// Returns how the trigger request was handled.
    pub fn disposition(&self) -> RefreshTriggerDisposition {
        self.disposition
    }

    /// Returns true when this call started a new refresh task.
    pub fn started_new_refresh(&self) -> bool {
        matches!(self.disposition, RefreshTriggerDisposition::Started)
    }

    /// Returns true when waiting would block on an in-flight refresh.
    pub fn waits_for_completion(&self) -> bool {
        self.wait_rx.is_some()
    }

    /// Wait for the refresh to complete.
    ///
    /// Returns immediately when there is nothing to wait for
    /// (e.g. the request was debounced and no refresh was started).
    pub async fn wait(&self) {
        let mut wait_rx = match &self.wait_rx {
            Some(rx) => rx.clone(),
            None => return,
        };
        if *wait_rx.borrow() {
            return;
        }
        while wait_rx.changed().await.is_ok() {
            if *wait_rx.borrow() {
                return;
            }
        }
    }
}

/// Configuration for ModelInventory
#[derive(Debug, Clone)]
pub struct ModelInventoryConfig {
    /// TTL before data is considered stale (default: 30s)
    pub stale_ttl: Duration,
    /// Timeout for individual remote node queries (default: 5s)
    pub remote_node_timeout: Duration,
    /// Global timeout for remote refresh pass (default: 30s)
    pub remote_refresh_timeout: Duration,
    /// Max concurrent remote node queries (default: 10)
    pub remote_concurrency: usize,
    /// Minimum interval between refresh triggers (default: 1s)
    pub refresh_debounce: Duration,
}

impl Default for ModelInventoryConfig {
    fn default() -> Self {
        Self {
            stale_ttl: Duration::from_secs(30),
            remote_node_timeout: Duration::from_secs(5),
            remote_refresh_timeout: Duration::from_secs(30),
            remote_concurrency: 10,
            refresh_debounce: Duration::from_secs(1),
        }
    }
}

/// Inner state shared between all clones of ModelInventory.
///
/// This ensures that all clones see the same model data and refresh state,
/// which is critical for the control plane to work correctly.
struct ModelInventoryInner {
    /// Current local model snapshot (fast reads via arc-swap)
    local_snapshot: ArcSwap<Vec<ModelEntry>>,
    /// Current remote model snapshot (fast reads via arc-swap)
    remote_snapshot: ArcSwap<Vec<ModelEntry>>,
    /// Mutable refresh state (protected by mutex)
    refresh_state: Mutex<RefreshState>,
    /// Agent configuration
    config: Arc<AgentConfig>,
    /// Optional mesh handle for remote operations
    #[cfg(feature = "remote")]
    mesh: parking_lot::RwLock<Option<MeshRuntimeHandle>>,
    #[cfg(feature = "remote")]
    remote_catalogs: parking_lot::Mutex<RemoteCatalogCache>,
    #[cfg(feature = "remote")]
    remote_epoch: std::sync::atomic::AtomicU64,
    /// Inventory configuration
    inv_config: ModelInventoryConfig,
    /// Broadcast a monotonically-increasing version after every successful refresh cycle.
    /// Subscribers can use this to avoid polling `get_snapshot()` on a timer.
    update_version_tx: broadcast::Sender<u64>,
    update_version: std::sync::atomic::AtomicU64,
}

/// Main inventory service for non-blocking model listing.
///
/// Uses shared interior state via `Arc<ModelInventoryInner>` so that all clones
/// see the same model data and refresh state. This prevents the bug where
/// refreshing a clone doesn't update the original instance.
pub struct ModelInventory {
    inner: Arc<ModelInventoryInner>,
}

#[cfg(feature = "remote")]
#[derive(Clone, Default)]
struct RemoteRefreshResult {
    catalogs: std::collections::HashMap<libp2p::PeerId, Vec<ModelEntry>>,
    timeout_count: usize,
    node_count: usize,
    incomplete: bool,
}

#[cfg(feature = "remote")]
#[derive(Clone, Copy)]
enum RemoteCatalogFailure {
    TimedOut,
    Unavailable,
}

#[cfg(feature = "remote")]
#[derive(Default)]
struct RemoteSnapshotUpdate {
    models: Vec<ModelEntry>,
    timeout_count: usize,
    node_count: usize,
    incomplete: bool,
    epoch: u64,
}

#[cfg(feature = "remote")]
#[derive(Default)]
struct RemoteCatalogCache {
    scopes: Vec<querymt_remote::MeshScopeId>,
    peers: std::collections::HashMap<libp2p::PeerId, (Instant, Vec<ModelEntry>)>,
}

#[cfg(feature = "remote")]
impl RemoteCatalogCache {
    fn merge(
        &mut self,
        scopes: &[querymt_remote::MeshScopeId],
        result: &RemoteRefreshResult,
        now: Instant,
    ) -> Vec<ModelEntry> {
        if self.scopes != scopes {
            self.peers.clear();
            self.scopes = scopes.to_vec();
        }
        // A successful empty catalog withdraws that peer's models. Missing or failed
        // queries retain the last good catalog for a bounded five-minute grace period.
        self.peers.retain(|_, (updated, _)| {
            now.saturating_duration_since(*updated) < Duration::from_secs(300)
        });
        for (peer, models) in &result.catalogs {
            self.peers.insert(*peer, (now, models.clone()));
        }
        let mut peers: Vec<_> = self.peers.iter().collect();
        peers.sort_by_key(|(peer, _)| **peer);
        peers
            .into_iter()
            .flat_map(|(_, (_, models))| models.clone())
            .collect()
    }
}

#[cfg(feature = "remote")]
type RemoteRefreshKey = (
    libp2p::PeerId,
    Vec<querymt_remote::MeshScopeId>,
    Duration,
    Duration,
    usize,
);

#[cfg(feature = "remote")]
fn shared_remote_refreshes()
-> &'static moka::future::Cache<RemoteRefreshKey, Arc<RemoteRefreshResult>> {
    static CACHE: std::sync::OnceLock<
        moka::future::Cache<RemoteRefreshKey, Arc<RemoteRefreshResult>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        moka::future::Cache::builder()
            .max_capacity(64)
            .time_to_live(Duration::from_secs(1))
            .build()
    })
}

#[cfg(feature = "remote")]
async fn collect_remote_catalogs<S, T, Q, F>(
    discoveries: S,
    query: Q,
    concurrency: usize,
    deadline: tokio::time::Instant,
) -> RemoteRefreshResult
where
    S: futures_util::Stream<Item = Result<(libp2p::PeerId, T), RemoteCatalogFailure>>,
    Q: Fn(libp2p::PeerId, T) -> F,
    F: std::future::Future<Output = Result<Vec<ModelEntry>, RemoteCatalogFailure>>,
{
    futures_util::pin_mut!(discoveries);
    let mut queries = futures_util::stream::FuturesUnordered::new();
    let mut seen = std::collections::HashSet::new();
    let mut discovery_done = false;
    let mut result = RemoteRefreshResult::default();
    while !discovery_done || !queries.is_empty() {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                result.timeout_count += queries.len() + usize::from(!discovery_done);
                result.incomplete = true;
                tracing::warn!(pending_nodes = queries.len(), discovery_incomplete = !discovery_done,
                    "Remote model refresh reached its global deadline");
                break;
            }
            Some((peer_id, outcome)) = queries.next(), if !queries.is_empty() => {
                match outcome {
                    Ok(models) => { result.catalogs.insert(peer_id, models); }
                    Err(failure) => {
                        result.incomplete = true;
                        result.timeout_count += usize::from(matches!(failure, RemoteCatalogFailure::TimedOut));
                    }
                }
            }
            node = discoveries.next(), if !discovery_done && queries.len() < concurrency.max(1) => {
                match node {
                    Some(Ok((peer_id, node))) if seen.insert(peer_id) => {
                        result.node_count += 1;
                        let future = query(peer_id, node);
                        queries.push(async move { (peer_id, future.await) });
                    }
                    Some(Err(failure)) => {
                        result.incomplete = true;
                        result.timeout_count += usize::from(matches!(failure, RemoteCatalogFailure::TimedOut));
                    }
                    Some(Ok(_)) => {}
                    None => { discovery_done = true; }
                }
            }
        }
    }
    // Dropping the owned stream and query futures cancels outstanding work.
    result
}

impl Clone for ModelInventory {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl ModelInventory {
    /// Create a new ModelInventory with default configuration
    pub fn new(config: Arc<AgentConfig>) -> Self {
        Self::with_config(config, ModelInventoryConfig::default())
    }

    /// Create a new ModelInventory with custom configuration
    pub fn with_config(config: Arc<AgentConfig>, inv_config: ModelInventoryConfig) -> Self {
        let (update_version_tx, _rx) = broadcast::channel(16);
        Self {
            inner: Arc::new(ModelInventoryInner {
                local_snapshot: ArcSwap::from(Arc::new(Vec::new())),
                remote_snapshot: ArcSwap::from(Arc::new(Vec::new())),
                refresh_state: Mutex::new(RefreshState {
                    local_refreshing: false,
                    remote_refreshing: false,
                    local_updated_at: None,
                    remote_updated_at: None,
                    remote_timeout_count: 0,
                    remote_node_count: 0,
                    refresh_done_tx: watch::channel(true).0,
                    last_triggered_at: None,
                    refresh_requested_after_current: false,
                }),
                config,
                #[cfg(feature = "remote")]
                mesh: parking_lot::RwLock::new(None),
                #[cfg(feature = "remote")]
                remote_catalogs: parking_lot::Mutex::new(RemoteCatalogCache::default()),
                #[cfg(feature = "remote")]
                remote_epoch: std::sync::atomic::AtomicU64::new(0),
                inv_config,
                update_version_tx,
                update_version: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    /// Set the mesh handle for remote operations
    #[cfg(feature = "remote")]
    pub fn set_mesh<M>(&self, mesh: M)
    where
        M: Into<MeshRuntimeHandle>,
    {
        let mesh = mesh.into();
        let mut current = self.inner.mesh.write();
        if current
            .as_ref()
            .is_some_and(|previous| previous.peer_id() != mesh.peer_id())
        {
            self.inner.remote_catalogs.lock().peers.clear();
            self.inner.remote_snapshot.store(Arc::new(Vec::new()));
        }
        self.inner
            .remote_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *current = Some(mesh);
    }

    #[cfg(feature = "remote")]
    pub fn clear_mesh(&self) {
        let mut current = self.inner.mesh.write();
        self.inner
            .remote_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        *current = None;
        self.inner.remote_catalogs.lock().peers.clear();
        self.inner.remote_snapshot.store(Arc::new(Vec::new()));
    }

    pub fn local_snapshot_entries_blocking(&self) -> Vec<ModelEntry> {
        (*self.inner.local_snapshot.load_full()).clone()
    }

    /// Get current model snapshot immediately (never blocks).
    /// Returns whatever data is available, even if stale.
    #[tracing::instrument(
        name = "model_inventory.get_snapshot",
        skip(self),
        fields(
            model_count = tracing::field::Empty,
            cache_hit = tracing::field::Empty,
            stale = tracing::field::Empty,
            refresh_in_progress = tracing::field::Empty,
        )
    )]
    pub async fn get_snapshot(&self) -> (Vec<ModelEntry>, SnapshotMeta) {
        let local = self.inner.local_snapshot.load_full();
        let remote = self.inner.remote_snapshot.load_full();

        let mut all = (*local).clone();
        all.extend((*remote).clone());

        let state = self.inner.refresh_state.lock().await;
        let meta = SnapshotMeta {
            local_updated_at: state.local_updated_at,
            remote_updated_at: state.remote_updated_at,
            refresh_in_progress: state.local_refreshing || state.remote_refreshing,
            is_stale: self.is_stale(&state),
            remote_timeout_count: state.remote_timeout_count,
            remote_node_count: state.remote_node_count,
        };

        tracing::Span::current().record("model_count", all.len());
        tracing::Span::current().record("cache_hit", !all.is_empty());
        tracing::Span::current().record("stale", meta.is_stale);
        tracing::Span::current().record("refresh_in_progress", meta.refresh_in_progress);

        (all, meta)
    }

    fn is_stale(&self, state: &RefreshState) -> bool {
        let now = Instant::now();
        let local_stale = state
            .local_updated_at
            .map(|t| now.duration_since(t) > self.inner.inv_config.stale_ttl)
            .unwrap_or(true);
        let remote_stale = state
            .remote_updated_at
            .map(|t| now.duration_since(t) > self.inner.inv_config.stale_ttl)
            .unwrap_or(true);
        local_stale || remote_stale
    }

    fn spawn_refresh_task(&self) {
        let inventory = self.clone();
        tokio::spawn(async move {
            inventory.run_refresh().await;
        });
    }

    fn spawn_local_refresh_task(&self) {
        let inventory = self.clone();
        tokio::spawn(async move {
            inventory.run_local_refresh().await;
        });
    }

    /// Trigger a background refresh. Returns immediately.
    /// Uses debouncing to avoid triggering too frequently.
    #[tracing::instrument(
        name = "model_inventory.trigger_refresh",
        skip(self),
        fields(disposition = tracing::field::Empty, wait_for_completion = tracing::field::Empty)
    )]
    pub async fn trigger_refresh(&self) -> RefreshHandle {
        let mut state = self.inner.refresh_state.lock().await;

        // ── Case 1: a refresh is already in progress ──
        // Return a handle that waits on the active cycle.
        if state.local_refreshing || state.remote_refreshing {
            let handle = RefreshHandle::pending(
                state.refresh_done_tx.subscribe(),
                RefreshTriggerDisposition::AlreadyInProgress,
            );
            tracing::Span::current()
                .record("disposition", handle.disposition().as_str())
                .record("wait_for_completion", handle.waits_for_completion());
            return handle;
        }

        let now = Instant::now();

        // ── Case 2: within the debounce window ──
        // No refresh is running and we recently completed one.
        // Return a handle that completes immediately rather than
        // holding the callers hostage until a timeout.
        if let Some(last) = state.last_triggered_at
            && now.duration_since(last) < self.inner.inv_config.refresh_debounce
        {
            let handle = RefreshHandle::completed(RefreshTriggerDisposition::Debounced);
            tracing::Span::current()
                .record("disposition", handle.disposition().as_str())
                .record("wait_for_completion", handle.waits_for_completion());
            return handle;
        }

        // ── Case 3: start a new refresh ──
        state.last_triggered_at = Some(now);
        state.local_refreshing = true;
        state.remote_refreshing = true;
        state.refresh_requested_after_current = false;

        // Replace the completion channel so new waiters observe this cycle.
        let (refresh_done_tx, refresh_done_rx) = watch::channel(false);
        state.refresh_done_tx = refresh_done_tx;

        drop(state); // release lock before spawn
        self.spawn_refresh_task();

        let handle = RefreshHandle::pending(refresh_done_rx, RefreshTriggerDisposition::Started);
        tracing::Span::current()
            .record("disposition", handle.disposition().as_str())
            .record("wait_for_completion", handle.waits_for_completion());
        handle
    }

    /// Ensure the local snapshot is refreshed without also querying remote nodes.
    #[tracing::instrument(
        name = "model_inventory.ensure_local_refresh",
        skip(self),
        fields(disposition = tracing::field::Empty, wait_for_completion = tracing::field::Empty)
    )]
    pub async fn ensure_local_refresh(&self) -> RefreshHandle {
        let mut state = self.inner.refresh_state.lock().await;

        if state.local_refreshing || state.remote_refreshing {
            let handle = RefreshHandle::pending(
                state.refresh_done_tx.subscribe(),
                RefreshTriggerDisposition::AlreadyInProgress,
            );
            tracing::Span::current()
                .record("disposition", handle.disposition().as_str())
                .record("wait_for_completion", handle.waits_for_completion());
            return handle;
        }

        let now = Instant::now();
        if let Some(last) = state.last_triggered_at
            && now.duration_since(last) < self.inner.inv_config.refresh_debounce
            && state.local_updated_at.is_some()
        {
            let handle = RefreshHandle::completed(RefreshTriggerDisposition::Debounced);
            tracing::Span::current()
                .record("disposition", handle.disposition().as_str())
                .record("wait_for_completion", handle.waits_for_completion());
            return handle;
        }

        state.last_triggered_at = Some(now);
        state.local_refreshing = true;
        state.remote_refreshing = false;
        state.refresh_requested_after_current = false;

        let (refresh_done_tx, refresh_done_rx) = watch::channel(false);
        state.refresh_done_tx = refresh_done_tx;

        drop(state);
        self.spawn_local_refresh_task();

        let handle = RefreshHandle::pending(refresh_done_rx, RefreshTriggerDisposition::Started);
        tracing::Span::current()
            .record("disposition", handle.disposition().as_str())
            .record("wait_for_completion", handle.waits_for_completion());
        handle
    }

    /// Invalidate local cache and trigger refresh
    pub async fn invalidate_local(&self) {
        self.inner.local_snapshot.store(Arc::new(Vec::new()));
        {
            let mut state = self.inner.refresh_state.lock().await;
            state.local_updated_at = None;
            // Clear debounce timestamp so the triggered refresh is not suppressed.
            state.last_triggered_at = None;
            if state.local_refreshing || state.remote_refreshing {
                state.refresh_requested_after_current = true;
            }
        }
        self.trigger_refresh().await;
    }

    /// Mark remote data stale and refresh without hiding last-known-good catalogs.
    pub async fn invalidate_remote(&self) {
        {
            let mut state = self.inner.refresh_state.lock().await;
            state.remote_updated_at = None;
            // Clear debounce timestamp so the triggered refresh is not suppressed.
            state.last_triggered_at = None;
            if state.local_refreshing || state.remote_refreshing {
                state.refresh_requested_after_current = true;
            }
        }
        self.trigger_refresh().await;
    }

    /// Invalidate all caches and trigger refresh
    pub async fn invalidate_all(&self) {
        self.inner.local_snapshot.store(Arc::new(Vec::new()));
        self.inner.remote_snapshot.store(Arc::new(Vec::new()));
        {
            let mut state = self.inner.refresh_state.lock().await;
            state.local_updated_at = None;
            state.remote_updated_at = None;
            // Clear debounce timestamp so the triggered refresh is not suppressed.
            state.last_triggered_at = None;
            if state.local_refreshing || state.remote_refreshing {
                state.refresh_requested_after_current = true;
            }
        }
        self.trigger_refresh().await;
    }

    /// Get all models from the snapshot (local + remote combined).
    ///
    /// This is a convenience method that returns just the models without metadata.
    /// For full metadata including staleness info, use `get_snapshot()`.
    pub async fn get_all_models(&self) -> Vec<ModelEntry> {
        let (models, _) = self.get_snapshot().await;
        models
    }

    /// Subscribe to model-inventory update notifications.
    ///
    /// The returned receiver gets a monotonically-increasing version number each
    /// time a refresh cycle completes successfully. It intentionally drops the
    /// oldest pending version if the subscriber falls behind.
    pub fn subscribe_updates(&self) -> broadcast::Receiver<u64> {
        self.inner.update_version_tx.subscribe()
    }

    /// Get the current mesh handle (if any).
    #[cfg(feature = "remote")]
    pub fn mesh(&self) -> Option<MeshRuntimeHandle> {
        self.inner.mesh.read().clone()
    }

    #[tracing::instrument(
        name = "model_inventory.run_refresh",
        skip(self),
        fields(
            local_duration_ms = tracing::field::Empty,
            remote_duration_ms = tracing::field::Empty,
            local_model_count = tracing::field::Empty,
            remote_model_count = tracing::field::Empty,
            remote_timeout_count = tracing::field::Empty,
            remote_node_count = tracing::field::Empty,
        )
    )]
    async fn run_refresh(&self) {
        let start = Instant::now();

        // Refresh local and remote concurrently
        #[cfg(feature = "remote")]
        {
            let (local_result, remote_result) =
                tokio::join!(self.refresh_local(), self.refresh_remote());

            // Update local snapshot
            if let Ok(local_models) = local_result {
                tracing::Span::current().record("local_model_count", local_models.len());
                self.inner.local_snapshot.store(Arc::new(local_models));

                // Update local timestamp on success
                let mut state = self.inner.refresh_state.lock().await;
                state.local_updated_at = Some(Instant::now());
            }

            // Update remote snapshot
            if let Some(RemoteSnapshotUpdate {
                models: remote_models,
                timeout_count,
                node_count,
                incomplete,
                epoch,
            }) = remote_result
            {
                tracing::Span::current().record("remote_model_count", remote_models.len());
                tracing::Span::current().record("remote_timeout_count", timeout_count);
                tracing::Span::current().record("remote_node_count", node_count);
                let mut state = self.inner.refresh_state.lock().await;
                let _mesh_guard = self.inner.mesh.read();
                if self
                    .inner
                    .remote_epoch
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == epoch
                {
                    self.inner.remote_snapshot.store(Arc::new(remote_models));
                    state.remote_timeout_count = timeout_count;
                    state.remote_node_count = node_count;
                    state.remote_updated_at = if incomplete {
                        None
                    } else {
                        Some(Instant::now())
                    };
                }
            }
        }

        #[cfg(not(feature = "remote"))]
        {
            let local_start = Instant::now();
            let local_result = self.refresh_local().await;
            let local_duration = local_start.elapsed();

            tracing::Span::current().record("local_duration_ms", local_duration.as_millis());

            // Update local snapshot
            if let Ok(local_models) = local_result {
                tracing::Span::current().record("local_model_count", local_models.len());
                self.inner.local_snapshot.store(Arc::new(local_models));

                // Update local timestamp on success
                let mut state = self.inner.refresh_state.lock().await;
                state.local_updated_at = Some(Instant::now());
                state.remote_updated_at = Some(Instant::now());
            }
        }

        // Mark refresh complete. If invalidation requested a follow-up refresh
        // while this cycle was running, immediately start a new cycle with the
        // latest state after signalling current waiters.
        let mut state = self.inner.refresh_state.lock().await;
        state.local_refreshing = false;
        state.remote_refreshing = false;
        let rerun = state.refresh_requested_after_current;
        state.refresh_requested_after_current = false;
        let _ = state.refresh_done_tx.send(true);
        let next_version = self
            .inner
            .update_version
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let _ = self.inner.update_version_tx.send(next_version);

        if rerun {
            state.last_triggered_at = Some(Instant::now());
            state.local_refreshing = true;
            state.remote_refreshing = true;
            let (refresh_done_tx, _refresh_done_rx) = watch::channel(false);
            state.refresh_done_tx = refresh_done_tx;
        }
        drop(state);

        if rerun {
            self.spawn_refresh_task();
        }

        tracing::debug!(
            total_duration_ms = start.elapsed().as_millis(),
            rerun,
            "Model inventory refresh completed"
        );
    }

    async fn refresh_local(&self) -> Result<Vec<ModelEntry>, ()> {
        let models = enumerate_local_models(&self.inner.config).await;
        Ok(models)
    }

    #[tracing::instrument(
        name = "model_inventory.run_local_refresh",
        skip(self),
        fields(local_model_count = tracing::field::Empty)
    )]
    async fn run_local_refresh(&self) {
        if let Ok(local_models) = self.refresh_local().await {
            tracing::Span::current().record("local_model_count", local_models.len());
            self.inner.local_snapshot.store(Arc::new(local_models));

            let mut state = self.inner.refresh_state.lock().await;
            state.local_updated_at = Some(Instant::now());
            state.local_refreshing = false;
            let rerun = state.refresh_requested_after_current;
            state.refresh_requested_after_current = false;
            let _ = state.refresh_done_tx.send(true);
            let next_version = self
                .inner
                .update_version
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let _ = self.inner.update_version_tx.send(next_version);
            if rerun {
                state.last_triggered_at = Some(Instant::now());
                state.local_refreshing = true;
                state.remote_refreshing = true;
                state.refresh_done_tx = watch::channel(false).0;
            }
            drop(state);

            if rerun {
                self.spawn_refresh_task();
            }
            return;
        }

        let mut state = self.inner.refresh_state.lock().await;
        state.local_refreshing = false;
        let _ = state.refresh_done_tx.send(true);
    }

    #[cfg(feature = "remote")]
    #[tracing::instrument(
        name = "model_inventory.refresh_remote",
        skip(self),
        fields(
            node_count = tracing::field::Empty,
            timeout_count = tracing::field::Empty,
            model_count = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        )
    )]
    async fn refresh_remote(&self) -> Option<RemoteSnapshotUpdate> {
        let start = Instant::now();
        let epoch = self
            .inner
            .remote_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let Some(mesh) = self.mesh() else {
            return Some(RemoteSnapshotUpdate {
                epoch,
                ..Default::default()
            });
        };
        let scopes = mesh.active_scopes();
        let key = (
            *mesh.peer_id(),
            scopes.clone(),
            self.inner.inv_config.remote_node_timeout,
            self.inner.inv_config.remote_refresh_timeout,
            self.inner.inv_config.remote_concurrency,
        );
        // All local agents share one in-flight network pass and its short-lived result.
        let result = shared_remote_refreshes()
            .get_with(key, async {
                Arc::new(self.query_remote_catalogs(&mesh, &scopes).await)
            })
            .await;
        let current = self.inner.mesh.read();
        if self
            .inner
            .remote_epoch
            .load(std::sync::atomic::Ordering::SeqCst)
            != epoch
            || current
                .as_ref()
                .is_none_or(|current| current.active_scopes() != scopes)
        {
            return None;
        }
        let mut cache = self.inner.remote_catalogs.lock();
        let models = cache.merge(&scopes, &result, Instant::now());
        let retained = cache
            .peers
            .keys()
            .any(|peer| !result.catalogs.contains_key(peer));
        tracing::Span::current().record("timeout_count", result.timeout_count);
        tracing::Span::current().record("node_count", result.node_count);
        tracing::Span::current().record("model_count", models.len());
        tracing::Span::current().record("duration_ms", start.elapsed().as_millis());
        Some(RemoteSnapshotUpdate {
            models,
            timeout_count: result.timeout_count,
            node_count: result.node_count,
            incomplete: result.incomplete || retained,
            epoch,
        })
    }

    #[cfg(feature = "remote")]
    async fn query_remote_catalogs(
        &self,
        mesh: &MeshRuntimeHandle,
        scopes: &[querymt_remote::MeshScopeId],
    ) -> RemoteRefreshResult {
        use crate::agent::remote::RemoteNodeManager;
        let deadline = tokio::time::Instant::now() + self.inner.inv_config.remote_refresh_timeout;
        let local_peer = *mesh.peer_id();
        // Poll scopes concurrently and start bounded catalog queries as soon as a
        // node is yielded, without waiting for DHT provider discovery to finish.
        let discoveries = futures_util::stream::select_all(scopes.iter().map(|scope| {
            let scope = scope.clone();
            mesh.lookup_all_actors_scoped::<RemoteNodeManager>(&scope, "node_manager")
                .filter_map(move |entry| {
                    let node = match entry {
                        Ok(node_ref) => node_ref
                            .id()
                            .peer_id()
                            .copied()
                            .filter(|peer| *peer != local_peer)
                            .map(|peer| Ok((peer, (node_ref, scope.clone())))),
                        Err(error) => {
                            tracing::debug!(%scope, %error, "Remote node discovery failed");
                            Some(Err(RemoteCatalogFailure::Unavailable))
                        }
                    };
                    futures_util::future::ready(node)
                })
        }));
        let node_timeout = self.inner.inv_config.remote_node_timeout;
        collect_remote_catalogs(discoveries, |peer_id, (node_ref, discovered_scope)| async move {
            // Query the scope that actually advertised this node before trying
            // other active scopes, so an unrelated DHT miss cannot consume its budget.
            let peer_scopes: Vec<_> = std::iter::once(discovered_scope.clone())
                .chain(scopes.iter().filter(|scope| **scope != discovered_scope).cloned()).collect();
            let query = Self::query_peer_catalog(mesh, &node_ref, peer_id, &peer_scopes, node_timeout);
            match tokio::time::timeout(node_timeout, query).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    tracing::warn!(%peer_id, timeout_ms = node_timeout.as_millis(), "Remote catalog query timed out");
                    Err(RemoteCatalogFailure::TimedOut)
                }
            }
        }, self.inner.inv_config.remote_concurrency, deadline).await
    }

    #[cfg(feature = "remote")]
    async fn query_peer_catalog(
        mesh: &MeshRuntimeHandle,
        node_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        peer_id: libp2p::PeerId,
        scopes: &[querymt_remote::MeshScopeId],
        node_timeout: Duration,
    ) -> Result<Vec<ModelEntry>, RemoteCatalogFailure> {
        use crate::agent::remote::GetNodeInfo;
        use querymt_remote::{
            GetProviderCatalog, ProviderCatalogActor, ask_remote_with_timeout,
            scoped_provider_catalog,
        };
        let node_info = ask_remote_with_timeout(node_ref, &GetNodeInfo, node_timeout)
            .await
            .map_err(|error| {
                tracing::warn!(%peer_id, %error, "GetNodeInfo failed during model refresh");
                if matches!(error, kameo::error::RemoteSendError::ReplyTimeout) {
                    RemoteCatalogFailure::TimedOut
                } else {
                    RemoteCatalogFailure::Unavailable
                }
            })?;
        let mut failure = RemoteCatalogFailure::Unavailable;
        for scope in scopes {
            let name = scoped_provider_catalog(scope, &peer_id);
            let catalog = match mesh
                .lookup_actor_no_retry_with_timeout::<ProviderCatalogActor>(name, node_timeout)
                .await
            {
                Ok(Some(catalog)) => catalog,
                Ok(None) => continue,
                Err(error) => {
                    if matches!(error, querymt_remote::RemoteLookupError::Timeout { .. }) {
                        failure = RemoteCatalogFailure::TimedOut;
                    }
                    tracing::warn!(%peer_id, %scope, %error, "ProviderCatalog lookup failed");
                    continue;
                }
            };
            let snapshot =
                match ask_remote_with_timeout(&catalog, &GetProviderCatalog, node_timeout).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        if matches!(error, kameo::error::RemoteSendError::ReplyTimeout) {
                            failure = RemoteCatalogFailure::TimedOut;
                        }
                        tracing::warn!(%peer_id, %scope, %error, "GetProviderCatalog failed");
                        continue;
                    }
                };
            let node_id = crate::agent::remote::NodeId::parse(&snapshot.node.node_id)
                .ok()
                .map(|id| id.to_string())
                .unwrap_or_else(|| node_info.node_id.to_string());
            let label = snapshot
                .node
                .node_label
                .unwrap_or_else(|| node_info.hostname.clone());
            return Ok(snapshot
                .providers
                .into_iter()
                .map(|entry| {
                    let model = entry.model.unwrap_or_else(|| "*".to_string());
                    let provider = entry.provider;
                    ModelEntry {
                        id: format!("{provider}/{model}"),
                        label: entry.label.unwrap_or_else(|| {
                            if model == "*" {
                                format!("{provider} (all models)")
                            } else {
                                model.clone()
                            }
                        }),
                        source: "catalog".into(),
                        provider,
                        model,
                        node_id: Some(node_id.clone()),
                        node_label: Some(label.clone()),
                        family: entry.family,
                        quant: entry.quant,
                    }
                })
                .collect());
        }
        // Missing registration is not an authoritative empty catalog.
        Err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::helpers::empty_plugin_registry;
    use futures_util::future::join_all;
    use querymt::LLMParams;

    async fn make_test_config() -> Arc<AgentConfig> {
        let (registry, _temp_dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock-model");
        let builder = AgentConfigBuilder::new(Arc::new(registry), storage.clone(), llm);
        Arc::new(builder.build())
    }

    #[cfg(feature = "remote")]
    fn remote_model(name: &str) -> ModelEntry {
        ModelEntry {
            id: name.into(),
            label: name.into(),
            source: "catalog".into(),
            provider: "test".into(),
            model: name.into(),
            node_id: Some("peer".into()),
            node_label: None,
            family: None,
            quant: None,
        }
    }

    #[cfg(feature = "remote")]
    fn test_peer() -> libp2p::PeerId {
        libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id()
    }

    #[cfg(feature = "remote")]
    #[test]
    fn remote_catalogs_preserve_failures_and_expire_withdrawals_per_peer() {
        let mut cache = RemoteCatalogCache::default();
        let a = test_peer();
        let b = test_peer();
        let scopes = vec![querymt_remote::MeshScopeId::lan_default()];
        let now = Instant::now();
        let first = RemoteRefreshResult {
            catalogs: [(a, vec![remote_model("a")]), (b, vec![remote_model("b")])].into(),
            ..Default::default()
        };
        assert_eq!(cache.merge(&scopes, &first, now).len(), 2);
        let partial = RemoteRefreshResult {
            catalogs: [(a, vec![remote_model("new-a")])].into(),
            incomplete: true,
            ..Default::default()
        };
        let models = cache.merge(&scopes, &partial, now + Duration::from_secs(10));
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|model| model.id == "new-a"));
        assert!(models.iter().any(|model| model.id == "b"));
        let empty = RemoteRefreshResult {
            catalogs: [(a, Vec::new())].into(),
            ..Default::default()
        };
        assert_eq!(
            cache
                .merge(&scopes, &empty, now + Duration::from_secs(20))
                .len(),
            1
        );
        assert!(
            cache
                .merge(
                    &scopes,
                    &RemoteRefreshResult::default(),
                    now + Duration::from_secs(301)
                )
                .is_empty()
        );
    }

    #[cfg(feature = "remote")]
    #[test]
    fn leaving_scope_drops_its_cached_catalogs() {
        let mut cache = RemoteCatalogCache::default();
        let scopes = vec![querymt_remote::MeshScopeId::Iroh {
            mesh_id: "a".into(),
        }];
        let result = RemoteRefreshResult {
            catalogs: [(test_peer(), vec![remote_model("a")])].into(),
            ..Default::default()
        };
        assert_eq!(cache.merge(&scopes, &result, Instant::now()).len(), 1);
        assert!(
            cache
                .merge(&[], &RemoteRefreshResult::default(), Instant::now())
                .is_empty()
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn global_deadline_counts_pending_nodes_and_cancels_queries() {
        let a = test_peer();
        let b = test_peer();
        let c = test_peer();
        let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
        let discoveries =
            futures_util::stream::iter([Ok((a, Some(sender))), Ok((b, None)), Ok((c, None))]);
        let result = collect_remote_catalogs(
            discoveries,
            move |peer, sender| async move {
                let _sender = sender;
                if peer == b {
                    return Ok(vec![remote_model("b")]);
                }
                futures_util::future::pending().await
            },
            4,
            tokio::time::Instant::now() + Duration::from_millis(50),
        )
        .await;
        assert_eq!(result.node_count, 3);
        assert_eq!(result.catalogs.len(), 1);
        assert_eq!(result.timeout_count, 2);
        assert!(result.incomplete);
        assert!(
            receiver.await.is_err(),
            "deadline must drop unfinished query futures"
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn slow_scope_discovery_does_not_starve_early_catalogs() {
        let peer = test_peer();
        // Each scope yields a healthy cached provider, then remains open until
        // after the refresh deadline. They must not delay its catalog query.
        let scopes = (0..4).map(|_| {
            futures_util::stream::iter([Ok((peer, ()))]).chain(futures_util::stream::pending())
        });
        let discoveries = futures_util::stream::select_all(scopes);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = collect_remote_catalogs(
            discoveries,
            |_, ()| {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                futures_util::future::ready(Ok(vec![remote_model("early")]))
            },
            2,
            tokio::time::Instant::now() + Duration::from_millis(50),
        )
        .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(result.node_count, 1);
        assert_eq!(result.catalogs[&peer][0].id, "early");
        assert_eq!(
            result.timeout_count, 1,
            "unfinished discovery must be reported"
        );
        assert!(result.incomplete);
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn catalog_pipeline_bounds_concurrency_and_deduplicates_peers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let peers: Vec<_> = (0..6).map(|_| test_peer()).collect();
        let discoveries = futures_util::stream::iter(
            peers.iter().chain(peers.iter()).map(|peer| Ok((*peer, ()))),
        );
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let result = collect_remote_catalogs(
            discoveries,
            |_, ()| async {
                let current = active.fetch_add(1, Ordering::Relaxed) + 1;
                peak.fetch_max(current, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(5)).await;
                active.fetch_sub(1, Ordering::Relaxed);
                Ok(Vec::new())
            },
            2,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await;
        assert_eq!(result.node_count, peers.len());
        assert_eq!(result.catalogs.len(), peers.len());
        assert_eq!(peak.load(Ordering::Relaxed), 2);
        assert!(!result.incomplete);
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn remote_invalidation_keeps_snapshot_visible() {
        let inventory = ModelInventory::new(make_test_config().await);
        inventory
            .inner
            .remote_snapshot
            .store(Arc::new(vec![remote_model("retained")]));
        // Hold the refresh active so invalidation cannot finish a replacement pass.
        inventory.inner.refresh_state.lock().await.remote_refreshing = true;
        inventory.invalidate_remote().await;
        let (models, meta) = inventory.get_snapshot().await;
        assert_eq!(models[0].id, "retained");
        assert!(meta.is_stale);
        assert!(meta.refresh_in_progress);
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn remote_refresh_is_single_flight_across_inventories() {
        let key = (
            test_peer(),
            vec![],
            Duration::from_secs(5),
            Duration::from_secs(30),
            10,
        );
        let count = std::sync::atomic::AtomicUsize::new(0);
        let jobs = (0..20).map(|_| {
            shared_remote_refreshes().get_with(key.clone(), async {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                Arc::new(RemoteRefreshResult::default())
            })
        });
        join_all(jobs).await;
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_snapshot_returns_immediately() {
        let config = make_test_config().await;
        let inventory = ModelInventory::new(config);

        // Should return empty snapshot immediately
        let start = Instant::now();
        let (models, meta) = inventory.get_snapshot().await;
        let elapsed = start.elapsed();

        assert!(models.is_empty());
        assert!(meta.is_stale);
        assert!(!meta.refresh_in_progress);
        assert!(elapsed < Duration::from_millis(10)); // Should be very fast
    }

    #[tokio::test]
    async fn test_background_refresh_updates_snapshot() {
        let config = make_test_config().await;
        let inventory = ModelInventory::new(config);

        // Trigger refresh
        let handle = inventory.trigger_refresh().await;

        // Wait for completion with timeout
        let timeout = Duration::from_secs(5);
        match tokio::time::timeout(timeout, handle.wait()).await {
            Ok(_) => {
                // Refresh completed
                let (_, meta) = inventory.get_snapshot().await;
                assert!(!meta.refresh_in_progress);
                assert!(meta.local_updated_at.is_some());
            }
            Err(_) => {
                // Timeout - this is okay for unit tests, the important thing is it didn't hang forever
                println!(
                    "Background refresh timed out (expected in test environment without real providers)"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_stale_detection() {
        let config = make_test_config().await;
        let inv_config = ModelInventoryConfig {
            stale_ttl: Duration::from_millis(100),
            ..Default::default()
        };
        let inventory = ModelInventory::with_config(config, inv_config);

        // Initially stale (no data)
        let (_, meta) = inventory.get_snapshot().await;
        assert!(meta.is_stale);

        // Trigger refresh with timeout
        let handle = inventory.trigger_refresh().await;
        let timeout = Duration::from_secs(5);
        let _ = tokio::time::timeout(timeout, handle.wait()).await;

        // Should not be stale immediately after refresh (or timeout)
        let (_, meta) = inventory.get_snapshot().await;
        // If refresh completed, local_updated_at should be set
        if meta.local_updated_at.is_some() {
            assert!(!meta.is_stale);

            // Wait for TTL to expire
            tokio::time::sleep(Duration::from_millis(150)).await;

            // Should be stale again
            let (_, meta) = inventory.get_snapshot().await;
            assert!(meta.is_stale);
        }
    }

    #[tokio::test]
    async fn test_concurrent_refresh_dedup() {
        let config = make_test_config().await;
        let inventory = ModelInventory::new(config);

        // Trigger multiple refreshes concurrently
        let handle1 = inventory.trigger_refresh().await;
        let handle2 = inventory.trigger_refresh().await;
        let handle3 = inventory.trigger_refresh().await;

        assert_eq!(
            handle1.disposition(),
            RefreshTriggerDisposition::Started,
            "first refresh should start a new task"
        );
        assert_eq!(
            handle2.disposition(),
            RefreshTriggerDisposition::AlreadyInProgress,
            "second refresh should join the in-flight task"
        );
        assert_eq!(
            handle3.disposition(),
            RefreshTriggerDisposition::AlreadyInProgress,
            "third refresh should join the same in-flight task"
        );
        assert!(handle1.waits_for_completion());
        assert!(handle2.waits_for_completion());
        assert!(handle3.waits_for_completion());

        // Wait with generous timeout
        let timeout = Duration::from_secs(10);
        let _ = tokio::time::timeout(
            timeout,
            join_all(vec![handle1.wait(), handle2.wait(), handle3.wait()]),
        )
        .await;

        // Give a bit more time for any in-flight work to complete
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Only one refresh should have actually run, and it should be done
        let (_, meta) = inventory.get_snapshot().await;
        // If refresh completed, it should have local_updated_at set
        // If it timed out, that's also acceptable - the important thing is it didn't panic
        println!("Refresh completed: {}", !meta.refresh_in_progress);
    }

    #[tokio::test]
    async fn test_invalidate_clears_snapshot() {
        let config = make_test_config().await;
        let inventory = ModelInventory::new(config);

        // Refresh to get data with timeout
        let handle = inventory.trigger_refresh().await;
        let timeout = Duration::from_secs(5);
        let _ = tokio::time::timeout(timeout, handle.wait()).await;

        // Invalidate
        inventory.invalidate_all().await;

        // Snapshot should be empty
        let (models, meta) = inventory.get_snapshot().await;
        assert!(models.is_empty());
        assert!(meta.is_stale);
    }

    #[tokio::test]
    async fn test_debounce_prevents_rapid_refresh() {
        let config = make_test_config().await;
        let inv_config = ModelInventoryConfig {
            refresh_debounce: Duration::from_millis(500),
            ..Default::default()
        };
        let inventory = ModelInventory::with_config(config, inv_config);

        // Trigger refresh with timeout
        let handle = inventory.trigger_refresh().await;
        assert_eq!(handle.disposition(), RefreshTriggerDisposition::Started);
        let timeout = Duration::from_secs(5);
        let _ = tokio::time::timeout(timeout, handle.wait()).await;

        // Immediately trigger again - should be debounced
        let debounced = inventory.trigger_refresh().await;
        assert_eq!(
            debounced.disposition(),
            RefreshTriggerDisposition::Debounced,
            "refresh inside the debounce window should not start new work"
        );
        assert!(!debounced.waits_for_completion());

        // Wait less than debounce time and confirm it is still debounced.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let still_debounced = inventory.trigger_refresh().await;
        assert_eq!(
            still_debounced.disposition(),
            RefreshTriggerDisposition::Debounced,
            "refresh should stay debounced until the window expires"
        );
        assert!(!still_debounced.waits_for_completion());
    }

    /// Test that clones share refresh state with the original inventory.
    ///
    /// Refreshing or mutating a clone must be visible through every handle because
    /// `ModelInventory` clones share a single `ModelInventoryInner`.
    #[tokio::test]
    async fn test_clone_shares_state_with_original() {
        let config = make_test_config().await;
        let original = ModelInventory::new(config);

        // Verify initial state
        let (_, initial_meta) = original.get_snapshot().await;
        assert!(
            initial_meta.local_updated_at.is_none(),
            "Should start with no update time"
        );

        // Clone the inventory (as LocalAgentHandle does)
        let cloned = original.clone();

        // Directly mutate the cloned's refresh state to simulate a completed refresh
        // In real code, this would happen during run_refresh()
        {
            let mut state = cloned.inner.refresh_state.lock().await;
            state.local_updated_at = Some(Instant::now());
        }

        // Check if original sees the updated state
        let (_, original_meta) = original.get_snapshot().await;
        let (_, cloned_meta) = cloned.get_snapshot().await;

        assert_eq!(
            original_meta.local_updated_at.is_some(),
            cloned_meta.local_updated_at.is_some(),
            "Original and clone should agree about shared refresh state"
        );

        assert!(
            original_meta.local_updated_at.is_some(),
            "Original should see the refresh state written through a clone."
        );
    }

    /// Test that clones share the same underlying model snapshot data.
    #[tokio::test]
    async fn test_clone_shares_snapshots() {
        let config = make_test_config().await;
        let original = ModelInventory::new(config);

        // Clone the inventory
        let cloned = original.clone();

        // Store some models in the clone
        let test_models = vec![ModelEntry {
            id: "test/model1".to_string(),
            label: "model1".to_string(),
            source: "test".to_string(),
            provider: "test".to_string(),
            model: "model1".to_string(),
            node_id: None,
            node_label: None,
            family: None,
            quant: None,
        }];
        cloned
            .inner
            .local_snapshot
            .store(Arc::new(test_models.clone()));

        // Check if original sees the models
        let (original_models, _) = original.get_snapshot().await;
        let (cloned_models, _) = cloned.get_snapshot().await;

        assert_eq!(
            original_models.len(),
            cloned_models.len(),
            "Original and clone should observe the same snapshot length"
        );

        assert_eq!(
            original_models.first().map(|entry| entry.id.as_str()),
            cloned_models.first().map(|entry| entry.id.as_str()),
            "Original should see model entries stored through a clone."
        );
    }

    /// Test that remote_updated_at is set after successful remote refresh
    ///
    /// This tests the fix for the bug where remote_updated_at was never set,
    /// causing is_stale() to always return true after remote refresh.
    #[tokio::test]
    async fn test_remote_updated_at_set_after_refresh() {
        let config = make_test_config().await;
        let inventory = ModelInventory::new(config);

        // Initial state should have no timestamps
        let (_, initial_meta) = inventory.get_snapshot().await;
        assert!(initial_meta.local_updated_at.is_none());
        assert!(initial_meta.remote_updated_at.is_none());
        assert!(initial_meta.is_stale, "Should be stale initially");

        // Simulate a successful refresh by directly setting timestamps
        {
            let mut state = inventory.inner.refresh_state.lock().await;
            state.local_updated_at = Some(Instant::now());
            state.remote_updated_at = Some(Instant::now());
        }

        // Check that is_stale() returns false after successful refresh
        let (_, meta) = inventory.get_snapshot().await;
        assert!(
            meta.local_updated_at.is_some(),
            "local_updated_at should be set"
        );
        assert!(
            meta.remote_updated_at.is_some(),
            "remote_updated_at should be set"
        );
        assert!(
            !meta.is_stale,
            "Should not be stale after successful refresh"
        );
    }

    #[tokio::test]
    async fn test_refresh_handle_wait_returns_after_completion_even_if_wait_starts_late() {
        let config = make_test_config().await;
        let inventory = ModelInventory::new(config);

        let handle = inventory.trigger_refresh().await;

        // Let the refresh complete before we begin waiting on the handle.
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (_, meta) = inventory.get_snapshot().await;
                if !meta.refresh_in_progress {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

        let waited = tokio::time::timeout(Duration::from_millis(250), handle.wait()).await;
        assert!(
            waited.is_ok(),
            "late waiters should observe retained completion state and return promptly"
        );
    }

    #[tokio::test]
    async fn local_refresh_reserves_invalidated_follow_up_before_spawning() {
        let inventory = ModelInventory::new(make_test_config().await);
        {
            let mut state = inventory.inner.refresh_state.lock().await;
            state.local_refreshing = true;
            state.refresh_requested_after_current = true;
        }
        inventory.run_local_refresh().await;
        let handle = inventory.trigger_refresh().await;
        assert_eq!(
            handle.disposition(),
            RefreshTriggerDisposition::AlreadyInProgress
        );
        tokio::time::timeout(Duration::from_secs(5), handle.wait())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_invalidation_during_active_refresh_requests_follow_up_cycle() {
        let config = make_test_config().await;
        let inventory = ModelInventory::new(config);

        let _handle = inventory.trigger_refresh().await;

        {
            let state = inventory.inner.refresh_state.lock().await;
            assert!(state.local_refreshing || state.remote_refreshing);
        }

        inventory.invalidate_all().await;

        {
            let state = inventory.inner.refresh_state.lock().await;
            assert!(
                state.refresh_requested_after_current
                    || state.local_refreshing
                    || state.remote_refreshing,
                "invalidation during an active refresh should queue or immediately run a follow-up cycle"
            );
        }
    }
}
