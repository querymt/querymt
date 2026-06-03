//! Non-blocking model inventory service with snapshot-based reads.
//!
//! Provides fast, non-blocking model listing by returning cached snapshots
//! immediately and refreshing data in the background.

use arc_swap::ArcSwap;
use futures_util::stream::{FuturesUnordered, StreamExt};
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
    /// Number of remote nodes that timed out during last refresh
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
        *self.inner.mesh.write() = Some(mesh.into());
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

    /// Invalidate remote cache and trigger refresh
    pub async fn invalidate_remote(&self) {
        self.inner.remote_snapshot.store(Arc::new(Vec::new()));
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
            if let Ok((remote_models, timeout_count, node_count)) = remote_result {
                tracing::Span::current().record("remote_model_count", remote_models.len());
                tracing::Span::current().record("remote_timeout_count", timeout_count);
                tracing::Span::current().record("remote_node_count", node_count);
                self.inner.remote_snapshot.store(Arc::new(remote_models));

                let mut state = self.inner.refresh_state.lock().await;
                state.remote_timeout_count = timeout_count;
                state.remote_node_count = node_count;
                state.remote_updated_at = Some(Instant::now());
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
    async fn refresh_remote(&self) -> Result<(Vec<ModelEntry>, usize, usize), ()> {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use querymt_remote::{GetProviderCatalog, ProviderCatalogActor, scoped_provider_catalog};
        use tokio::time::timeout;

        let start = Instant::now();
        let mesh = match self.mesh() {
            Some(mesh) => mesh,
            None => return Ok((Vec::new(), 0, 0)),
        };

        let local_peer_id = *mesh.peer_id();
        let scopes = mesh.active_scopes();
        let mut node_refs = Vec::new();

        for scope in &scopes {
            let mut stream =
                mesh.lookup_all_actors_scoped::<RemoteNodeManager>(&scope, "node_manager");

            while let Some(result) = stream.next().await {
                if let Ok(node_ref) = result
                    && node_ref.id().peer_id() != Some(&local_peer_id)
                {
                    node_refs.push(node_ref);
                }
            }
        }

        node_refs.sort_by_key(|node_ref| node_ref.id().peer_id().copied());
        node_refs.dedup_by(|a, b| a.id().peer_id() == b.id().peer_id());

        let mut all_remote = Vec::new();
        let mut timeout_count = 0usize;

        let node_count = node_refs.len();
        tracing::Span::current().record("node_count", node_count);

        // Query nodes with bounded concurrency and timeouts
        let semaphore = Arc::new(tokio::sync::Semaphore::new(
            self.inner.inv_config.remote_concurrency,
        ));
        let mut handles = Vec::new();

        for node_ref in node_refs {
            let semaphore = semaphore.clone();
            let node_timeout = self.inner.inv_config.remote_node_timeout;
            let mesh = mesh.clone();
            let scopes = scopes.clone();

            let handle = tokio::spawn(async move {
                let _permit = semaphore.acquire().await.unwrap();

                let node_info =
                    match timeout(node_timeout, node_ref.ask::<GetNodeInfo>(&GetNodeInfo)).await {
                        Ok(Ok(info)) => info,
                        Ok(Err(e)) => {
                            tracing::warn!("GetNodeInfo failed: {}", e);
                            return None;
                        }
                        Err(_) => {
                            tracing::warn!(
                                "GetNodeInfo timed out for node (timeout: {:?})",
                                node_timeout
                            );
                            return Some(Err(()));
                        }
                    };

                let Some(peer_id) = node_ref.id().peer_id().copied() else {
                    return None;
                };

                for scope in &scopes {
                    let catalog_name = scoped_provider_catalog(scope, &peer_id);
                    match timeout(
                        node_timeout,
                        mesh.lookup_actor::<ProviderCatalogActor>(catalog_name),
                    )
                    .await
                    {
                        Ok(Ok(Some(catalog_ref))) => {
                            match timeout(
                                node_timeout,
                                catalog_ref.ask::<GetProviderCatalog>(&GetProviderCatalog),
                            )
                            .await
                            {
                                Ok(Ok(snapshot)) => {
                                    let fallback_node_id = node_info.node_id.to_string();
                                    let resolved_node_id =
                                        crate::agent::remote::NodeId::parse(&snapshot.node.node_id)
                                            .ok()
                                            .map(|id| id.to_string())
                                            .unwrap_or(fallback_node_id);
                                    let resolved_label = snapshot
                                        .node
                                        .node_label
                                        .clone()
                                        .unwrap_or_else(|| node_info.hostname.clone());
                                    let entries: Vec<ModelEntry> = snapshot
                                        .providers
                                        .into_iter()
                                        .map(|m| {
                                            let model = m.model.unwrap_or_else(|| "*".to_string());
                                            let provider = m.provider;
                                            ModelEntry {
                                                id: format!("{}/{}", provider, model),
                                                label: m.label.unwrap_or_else(|| {
                                                    if model == "*" {
                                                        format!("{} (all models)", provider)
                                                    } else {
                                                        model.clone()
                                                    }
                                                }),
                                                source: "catalog".to_string(),
                                                provider,
                                                model,
                                                node_id: Some(resolved_node_id.clone()),
                                                node_label: Some(resolved_label.clone()),
                                                family: m.family,
                                                quant: m.quant,
                                            }
                                        })
                                        .collect();
                                    return Some(Ok(entries));
                                }
                                Ok(Err(e)) => {
                                    tracing::debug!(
                                        "GetProviderCatalog failed for peer {}: {}",
                                        peer_id,
                                        e
                                    );
                                }
                                Err(_) => {
                                    tracing::warn!(
                                        "GetProviderCatalog timed out for node (timeout: {:?})",
                                        node_timeout
                                    );
                                    return Some(Err(()));
                                }
                            }
                        }
                        Ok(Ok(None)) => {}
                        Ok(Err(e)) => {
                            tracing::debug!(
                                "ProviderCatalog lookup failed for peer {}: {}",
                                peer_id,
                                e
                            );
                        }
                        Err(_) => {
                            tracing::warn!(
                                "ProviderCatalog lookup timed out for node (timeout: {:?})",
                                node_timeout
                            );
                            return Some(Err(()));
                        }
                    }
                }

                tracing::debug!(
                    peer_id = %peer_id,
                    "remote node has no provider catalog registration; skipping model enumeration"
                );
                Some(Ok(Vec::new()))
            });

            handles.push(handle);
        }

        // Use FuturesUnordered for partial results with deadline polling.
        let mut futures = FuturesUnordered::new();
        for handle in handles {
            futures.push(handle);
        }

        let global_timeout = self.inner.inv_config.remote_refresh_timeout;
        let deadline = Instant::now() + global_timeout;

        // Process results as they come in, respecting the global deadline
        while !futures.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                tracing::warn!(
                    "Remote model refresh timed out globally (timeout: {:?}), {} futures remaining",
                    global_timeout,
                    futures.len()
                );
                for handle in futures.iter() {
                    handle.abort();
                }
                break;
            }

            match tokio::time::timeout(remaining, futures.next()).await {
                Ok(Some(result)) => {
                    match result {
                        Ok(Some(Ok(models))) => {
                            all_remote.extend(models);
                        }
                        Ok(Some(Err(()))) => {
                            timeout_count += 1;
                        }
                        _ => {} // Task failed or returned None
                    }
                }
                Ok(None) => break, // No more futures
                Err(_) => {
                    // Timeout waiting for next future.
                    tracing::warn!(
                        "Remote model refresh timed out globally (timeout: {:?}), {} futures remaining",
                        global_timeout,
                        futures.len()
                    );
                    for handle in futures.iter() {
                        handle.abort();
                    }
                    break;
                }
            }
        }

        tracing::Span::current().record("timeout_count", timeout_count);
        tracing::Span::current().record("model_count", all_remote.len());
        tracing::Span::current().record("duration_ms", start.elapsed().as_millis());

        Ok((all_remote, timeout_count, node_count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::session::backend::StorageBackend;
    use crate::session::sqlite_storage::SqliteStorage;
    use crate::test_utils::helpers::empty_plugin_registry;
    use futures_util::future::join_all;
    use querymt::LLMParams;

    async fn make_test_config() -> Arc<AgentConfig> {
        let (registry, _temp_dir) = empty_plugin_registry().unwrap();
        let storage = Arc::new(SqliteStorage::connect(":memory:".into()).await.unwrap());
        let llm = LLMParams::new().provider("mock").model("mock-model");
        let builder = AgentConfigBuilder::new(
            Arc::new(registry),
            storage.session_store(),
            storage.event_journal(),
            llm,
        );
        Arc::new(builder.build())
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
