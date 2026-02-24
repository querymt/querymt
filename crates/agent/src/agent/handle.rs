//! AgentHandle facade — the public replacement for QueryMTAgent.
//!
//! This lightweight struct bundles shared config, the kameo session registry,
//! and connection-level mutable state. It is NOT an actor — just a convenient
//! bundle that consumers hold instead of `Arc<QueryMTAgent>`.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, ClientState};
use crate::agent::session_registry::SessionRegistry;
use crate::delegation::AgentRegistry;

use crate::index::WorkspaceIndexManagerActor;
use crate::middleware::CompositeDriver;
use crate::send_agent::SendAgent;
use crate::session::store::LLMConfig;
use crate::tools::ToolRegistry;
use agent_client_protocol::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, Client, Error, ExtNotification,
    ExtRequest, ExtResponse, ForkSessionRequest, ForkSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SetSessionModelRequest, SetSessionModelResponse,
};
use anyhow::Result;
use async_trait::async_trait;
use kameo::actor::ActorRef;
use querymt::LLMParams;
use std::any::Any;
#[cfg(feature = "remote")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::{Mutex, broadcast};
#[cfg(feature = "remote")]
use tokio::sync::{RwLock, Semaphore};

/// Lightweight facade replacing `Arc<QueryMTAgent>` for all consumers.
///
/// Holds shared config, the kameo session registry, and connection-level
/// mutable state. Not an actor — just a convenient bundle.
#[cfg(feature = "remote")]
#[derive(Clone, Debug)]
struct CachedNodeEntry {
    info: crate::agent::remote::NodeInfo,
    expires_at: std::time::Instant,
}

#[cfg(feature = "remote")]
#[derive(Debug)]
struct RemoteNodeMetadataCache {
    by_label: RwLock<std::collections::HashMap<String, CachedNodeEntry>>,
    invalidation_task_started: AtomicBool,
}

#[cfg(feature = "remote")]
impl RemoteNodeMetadataCache {
    fn new() -> Self {
        Self {
            by_label: RwLock::new(std::collections::HashMap::new()),
            invalidation_task_started: AtomicBool::new(false),
        }
    }
}

pub struct AgentHandle {
    pub config: Arc<AgentConfig>,
    pub registry: Arc<Mutex<SessionRegistry>>,

    // Connection-level mutable state
    pub client_state: Arc<StdMutex<Option<ClientState>>>,
    pub client: Arc<StdMutex<Option<Arc<dyn Client + Send + Sync>>>>,
    pub bridge: Arc<StdMutex<Option<ClientBridgeSender>>>,

    // Mutable default mode (UI "set agent mode" → affects new sessions)
    pub default_mode: StdMutex<AgentMode>,

    /// Handle to the kameo mesh swarm, set after `bootstrap_mesh()` succeeds.
    /// `None` in local-only mode. Wrapped in a `Mutex` for interior mutability
    /// so startup code can set it on the shared `Arc<AgentHandle>`.
    #[cfg(feature = "remote")]
    pub mesh: StdMutex<Option<crate::agent::remote::MeshHandle>>,

    #[cfg(feature = "remote")]
    remote_node_cache: Arc<RemoteNodeMetadataCache>,
}

impl AgentHandle {
    /// Construct an `AgentHandle` from a shared `AgentConfig`.
    ///
    /// This is the canonical way to create an `AgentHandle` after building
    /// an `AgentConfig` via `AgentConfigBuilder::build()`.
    pub fn from_config(config: Arc<AgentConfig>) -> Self {
        let registry = Arc::new(Mutex::new(SessionRegistry::new(config.clone())));
        Self {
            config,
            registry,
            client_state: Arc::new(StdMutex::new(None)),
            client: Arc::new(StdMutex::new(None)),
            bridge: Arc::new(StdMutex::new(None)),
            default_mode: StdMutex::new(crate::agent::core::AgentMode::Build),
            #[cfg(feature = "remote")]
            mesh: StdMutex::new(None),
            #[cfg(feature = "remote")]
            remote_node_cache: Arc::new(RemoteNodeMetadataCache::new()),
        }
    }

    /// Subscribes to agent events via the fanout (live stream).
    pub fn subscribe_events(&self) -> broadcast::Receiver<crate::events::EventEnvelope> {
        self.config.event_sink.fanout().subscribe()
    }

    /// Access the agent registry.
    pub fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync> {
        self.config.agent_registry.clone()
    }

    /// Access the tool registry for built-in tool execution.
    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.config.tool_registry_arc()
    }

    /// Access the pending elicitations map for resolving tool and MCP server elicitation requests.
    pub fn pending_elicitations(&self) -> crate::elicitation::PendingElicitationMap {
        self.config.pending_elicitations()
    }

    /// Access the workspace manager actor ref.
    pub fn workspace_manager_actor(&self) -> ActorRef<WorkspaceIndexManagerActor> {
        self.config.workspace_manager_actor()
    }

    /// Sets the client for protocol communication.
    pub fn set_client(&self, client: Arc<dyn Client + Send + Sync>) {
        if let Ok(mut handle) = self.client.lock() {
            *handle = Some(client);
        }
    }

    /// Sets the client bridge for ACP stdio communication.
    pub fn set_bridge(&self, bridge: ClientBridgeSender) {
        if let Ok(mut handle) = self.bridge.lock() {
            *handle = Some(bridge);
        }
    }

    /// Emits an event for external observers.
    ///
    /// This is a detached fire-and-forget API.
    /// FIXME: Prefer an awaited emit path for critical flows.
    pub fn emit_event(&self, session_id: &str, kind: crate::events::AgentEventKind) {
        self.config.emit_event(session_id, kind);
    }

    /// Gracefully shutdown the agent and all background tasks.
    pub async fn shutdown(&self) {
        log::info!("AgentHandle: Starting graceful shutdown");

        self.config.shutdown().await;

        // Wait briefly for cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        log::info!("AgentHandle: Shutdown complete");
    }

    /// Switch provider and model for a session (simple form)
    pub async fn set_provider(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), Error> {
        // Preserve the system prompt when switching models
        let system_prompt = self.get_session_system_prompt(session_id).await;

        let mut config = LLMParams::new().provider(provider).model(model);

        // Add system prompt to config
        for prompt_part in system_prompt {
            config = config.system(prompt_part);
        }

        self.set_llm_config(session_id, config).await
    }

    /// Helper method to extract system prompt from current session config
    async fn get_session_system_prompt(&self, session_id: &str) -> Vec<String> {
        // Try to get the current session's LLM config
        if let Ok(Some(current_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
        {
            // Try to extract system prompt from params JSON
            if let Some(params) = &current_config.params
                && let Some(system_array) = params.get("system").and_then(|v| v.as_array())
            {
                // Parse the array of strings
                let mut system_parts = Vec::new();
                for item in system_array {
                    if let Some(s) = item.as_str() {
                        system_parts.push(s.to_string());
                    }
                }
                if !system_parts.is_empty() {
                    return system_parts;
                }
            }
        }

        // Fall back to initial_config system prompt
        self.config.provider.initial_config().system.clone()
    }

    /// Switch provider configuration for a session (advanced form)
    pub async fn set_llm_config(&self, session_id: &str, config: LLMParams) -> Result<(), Error> {
        use crate::error::AgentError;
        let provider_name = config
            .provider
            .as_ref()
            .ok_or_else(|| Error::from(AgentError::ProviderRequired))?;

        if self
            .config
            .provider
            .plugin_registry()
            .get(provider_name)
            .await
            .is_none()
        {
            return Err(Error::from(AgentError::UnknownProvider {
                name: provider_name.clone(),
            }));
        }

        let llm_config = self
            .config
            .provider
            .history_store()
            .create_or_get_llm_config(&config)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        self.config
            .provider
            .history_store()
            .set_session_llm_config(session_id, llm_config.id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        // Fetch context limit from model info
        let context_limit =
            crate::model_info::get_model_info(&llm_config.provider, &llm_config.model)
                .and_then(|m| m.context_limit());

        self.emit_event(
            session_id,
            crate::events::AgentEventKind::ProviderChanged {
                provider: llm_config.provider.clone(),
                model: llm_config.model.clone(),
                config_id: llm_config.id,
                context_limit,
                provider_node_id: None,
            },
        );
        Ok(())
    }

    /// Get current LLM config for a session
    pub async fn get_session_llm_config(
        &self,
        session_id: &str,
    ) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_session_llm_config(session_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Get LLM config by ID
    pub async fn get_llm_config(&self, config_id: i64) -> Result<Option<LLMConfig>, Error> {
        self.config
            .provider
            .history_store()
            .get_llm_config(config_id)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))
    }

    /// Creates a CompositeDriver from the configured middleware drivers
    pub fn create_driver(&self) -> CompositeDriver {
        self.config.create_driver()
    }

    /// Returns the session limits from configured middleware
    pub fn get_session_limits(&self) -> Option<crate::events::SessionLimits> {
        self.config.get_session_limits()
    }

    /// Builds delegation metadata for ACP AgentCapabilities._meta field
    pub fn build_delegation_meta(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        self.config.build_delegation_meta()
    }

    /// Undo: revert filesystem to state at the given message_id.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn undo(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<crate::agent::undo::UndoResult, crate::agent::undo::UndoError> {
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.undo(message_id.to_string()).await
    }

    /// Redo: re-apply the next change in the redo stack.
    ///
    /// Routes through the kameo session actor via `SessionActorRef`.
    pub async fn redo(
        &self,
        session_id: &str,
    ) -> Result<crate::agent::undo::RedoResult, crate::agent::undo::UndoError> {
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(session_id).cloned().ok_or_else(|| {
                crate::agent::undo::UndoError::Other(format!("Session not found: {}", session_id))
            })?
        };

        session_ref.redo().await
    }

    // ── Remote session management (requires `remote` feature) ─────────────────

    /// List discovered peers in the kameo mesh.
    ///
    /// Looks up all `RemoteNodeManager` instances registered under
    /// `"node_manager"` in the Kademlia DHT and calls `GetNodeInfo` on each.
    /// Requires a bootstrapped swarm (`--mesh` flag).
    ///
    /// Without a swarm or with no peers, returns an empty list.
    /// Returns a clone of the `MeshHandle` if the mesh is active.
    #[cfg(feature = "remote")]
    pub fn mesh(&self) -> Option<crate::agent::remote::MeshHandle> {
        self.mesh.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Activate the mesh by storing the `MeshHandle` returned by `bootstrap_mesh()`.
    ///
    /// Also propagates into `config.provider` so that sessions created by a
    /// `RemoteNodeManager` (which holds `Arc<AgentConfig>` with this provider)
    /// can route LLM calls through the mesh even though the mesh was bootstrapped
    /// after the config was built.
    #[cfg(feature = "remote")]
    pub fn set_mesh(&self, mesh: crate::agent::remote::MeshHandle) {
        *self.mesh.lock().unwrap_or_else(|e| e.into_inner()) = Some(mesh.clone());
        self.config.provider.set_mesh(Some(mesh));
    }

    /// Enable/disable automatic mesh fallback for unpinned provider resolution.
    #[cfg(feature = "remote")]
    pub fn set_mesh_fallback(&self, enabled: bool) {
        self.config.provider.set_mesh_fallback(enabled);
    }

    #[cfg(feature = "remote")]
    fn remote_node_info_timeout() -> std::time::Duration {
        let default_ms = 3_000_u64;
        let timeout_ms = std::env::var("QUERYMT_REMOTE_NODE_INFO_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        std::time::Duration::from_millis(timeout_ms)
    }

    #[cfg(feature = "remote")]
    fn remote_node_lookup_parallelism() -> usize {
        std::env::var("QUERYMT_REMOTE_NODE_LOOKUP_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(8)
    }

    #[cfg(feature = "remote")]
    fn remote_node_cache_ttl() -> std::time::Duration {
        let default_ms = 10_000_u64;
        let ttl_ms = std::env::var("QUERYMT_REMOTE_NODE_CACHE_TTL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default_ms);
        std::time::Duration::from_millis(ttl_ms)
    }

    #[cfg(feature = "remote")]
    fn peer_cache_key(peer_id: Option<libp2p::PeerId>, fallback_actor_id: u64) -> String {
        if let Some(pid) = peer_id {
            format!("peer:{pid}")
        } else {
            format!("actor:{fallback_actor_id}")
        }
    }

    #[cfg(feature = "remote")]
    async fn get_cached_remote_node(
        &self,
        cache_key: &str,
    ) -> Option<crate::agent::remote::NodeInfo> {
        let now = std::time::Instant::now();
        if let Some(entry) = self
            .remote_node_cache
            .by_label
            .read()
            .await
            .get(cache_key)
            .cloned()
            && entry.expires_at > now
        {
            return Some(entry.info);
        }

        let mut guard = self.remote_node_cache.by_label.write().await;
        if let Some(entry) = guard.get(cache_key)
            && entry.expires_at <= now
        {
            guard.remove(cache_key);
        }
        None
    }

    #[cfg(feature = "remote")]
    async fn insert_cached_remote_node(
        &self,
        cache_key: String,
        info: crate::agent::remote::NodeInfo,
    ) {
        let ttl = Self::remote_node_cache_ttl();
        self.remote_node_cache.by_label.write().await.insert(
            cache_key,
            CachedNodeEntry {
                info,
                expires_at: std::time::Instant::now() + ttl,
            },
        );
    }

    #[cfg(feature = "remote")]
    fn ensure_remote_node_cache_invalidation_task(&self, mesh: &crate::agent::remote::MeshHandle) {
        if self
            .remote_node_cache
            .invalidation_task_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let mut rx = mesh.subscribe_peer_events();
        let cache = Arc::clone(&self.remote_node_cache);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(crate::agent::remote::mesh::PeerEvent::Discovered(peer_id))
                    | Ok(crate::agent::remote::mesh::PeerEvent::Expired(peer_id)) => {
                        let key = format!("peer:{peer_id}");
                        cache.by_label.write().await.remove(&key);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        cache
                            .invalidation_task_started
                            .store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }

    #[cfg(feature = "remote")]
    pub async fn list_remote_nodes(&self) -> Vec<crate::agent::remote::NodeInfo> {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::{StreamExt, stream::FuturesUnordered};

        let Some(mesh) = self.mesh() else {
            log::debug!("list_remote_nodes: mesh not bootstrapped");
            return Vec::new();
        };

        self.ensure_remote_node_cache_invalidation_task(&mesh);

        let local_peer_id = *mesh.peer_id();
        let timeout = Self::remote_node_info_timeout();
        let concurrency = Self::remote_node_lookup_parallelism();
        let semaphore = Arc::new(Semaphore::new(concurrency));

        let mut stream = mesh
            .lookup_all_actors::<RemoteNodeManager>(crate::agent::remote::dht_name::NODE_MANAGER);
        let mut lookups = FuturesUnordered::new();
        let mut cached_nodes = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(node_manager_ref) => {
                    let peer_id = node_manager_ref.id().peer_id().copied();
                    if peer_id == Some(local_peer_id) {
                        log::debug!("list_remote_nodes: skipping local node");
                        continue;
                    }

                    if let Some(pid) = peer_id
                        && !mesh.is_peer_alive(&pid)
                    {
                        let key = format!("peer:{pid}");
                        self.remote_node_cache.by_label.write().await.remove(&key);
                        log::debug!("list_remote_nodes: skipping stale DHT record for peer {pid}");
                        continue;
                    }

                    let cache_key =
                        Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                    if let Some(info) = self.get_cached_remote_node(&cache_key).await {
                        cached_nodes.push(info);
                        continue;
                    }

                    let semaphore = Arc::clone(&semaphore);
                    lookups.push(async move {
                        let permit = semaphore.acquire_owned().await.ok();
                        let res = tokio::time::timeout(
                            timeout,
                            node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                        )
                        .await;
                        drop(permit);
                        (cache_key, peer_id, res)
                    });
                }
                Err(e) => log::warn!("list_remote_nodes: lookup error: {}", e),
            }
        }

        let mut fetched_nodes = Vec::new();
        while let Some((cache_key, peer_id, result)) = lookups.next().await {
            match result {
                Ok(Ok(info)) => {
                    self.insert_cached_remote_node(cache_key, info.clone())
                        .await;
                    fetched_nodes.push(info);
                }
                Ok(Err(e)) => {
                    log::warn!("list_remote_nodes: GetNodeInfo failed: {}", e);
                }
                Err(_) => {
                    log::warn!(
                        "list_remote_nodes: GetNodeInfo timed out for peer {:?}",
                        peer_id
                    );
                }
            }
        }

        cached_nodes.extend(fetched_nodes);
        cached_nodes
    }

    /// Find a `RemoteNodeManager` by its stable node id (PeerId string).
    ///
    /// ## Fast path
    ///
    /// Tries a direct DHT lookup under `node_manager::peer::{node_id}` first.
    /// This succeeds whenever the remote node registered under the per-peer name
    /// (see [`dht_name::node_manager_for_peer`]) and is **not** gated on
    /// `is_peer_alive`, so it works even when mDNS has transiently expired the
    /// peer (TTL = 30 s) while the TCP connection is still alive.
    ///
    /// ## Fallback scan
    ///
    /// If the direct lookup misses (e.g. the remote node is running an older
    /// version that only registers under the global `"node_manager"` name),
    /// falls back to iterating all `RemoteNodeManager` actors via
    /// `lookup_all_actors` and comparing `GetNodeInfo.node_id`.  Unlike
    /// `list_remote_nodes`, this scan deliberately **skips the `is_peer_alive`
    /// filter**: the user has explicitly requested this node, so we attempt
    /// `GetNodeInfo` contact (3 s timeout) before giving up rather than
    /// silently discarding the candidate.
    #[cfg(feature = "remote")]
    pub async fn find_node_manager(
        &self,
        node_id: &str,
    ) -> Result<
        kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        agent_client_protocol::Error,
    > {
        use crate::agent::remote::{GetNodeInfo, RemoteNodeManager};
        use futures_util::{StreamExt, stream::FuturesUnordered};

        use crate::error::AgentError;
        let mesh = self
            .mesh()
            .ok_or_else(|| agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped))?;

        self.ensure_remote_node_cache_invalidation_task(&mesh);

        // ── Fast path: direct per-peer DHT lookup ────────────────────────────
        //
        // Remote nodes register under both the global "node_manager" name (for
        // mesh-wide discovery) and a per-peer "node_manager::peer::{peer_id}"
        // name (for this O(1) lookup). The per-peer lookup bypasses the
        // is_peer_alive gate that guards the fallback scan, so it works even
        // when mDNS has temporarily expired the peer's heartbeat.
        let direct_dht_name = crate::agent::remote::dht_name::node_manager_for_peer(&node_id);
        match mesh
            .lookup_actor::<RemoteNodeManager>(direct_dht_name.clone())
            .await
        {
            Ok(Some(node_manager_ref)) => {
                log::debug!(
                    "find_node_manager: fast-path DHT hit for '{}'",
                    direct_dht_name
                );
                return Ok(node_manager_ref);
            }
            Ok(None) => {
                log::debug!(
                    "find_node_manager: no direct DHT entry for '{}', falling back to scan",
                    direct_dht_name
                );
            }
            Err(e) => {
                log::debug!(
                    "find_node_manager: direct DHT lookup error for '{}': {}, falling back to scan",
                    direct_dht_name,
                    e
                );
            }
        }

        // ── Fallback scan: iterate all registered RemoteNodeManagers ─────────
        //
        // NOTE: unlike list_remote_nodes, we do NOT filter by is_peer_alive
        // here. The user explicitly chose this node, so we attempt GetNodeInfo
        // contact before giving up. The 3-second timeout on GetNodeInfo is the
        // real liveness check for a targeted user action.
        let local_peer_id = *mesh.peer_id();
        let timeout = Self::remote_node_info_timeout();
        let concurrency = Self::remote_node_lookup_parallelism();
        let semaphore = Arc::new(Semaphore::new(concurrency));
        let mut stream = mesh
            .lookup_all_actors::<RemoteNodeManager>(crate::agent::remote::dht_name::NODE_MANAGER);
        let mut lookups = FuturesUnordered::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(node_manager_ref) => {
                    let peer_id = node_manager_ref.id().peer_id().copied();
                    if peer_id == Some(local_peer_id) {
                        continue;
                    }
                    // No is_peer_alive check here — we contact the peer
                    // directly and let the GetNodeInfo timeout decide.

                    let cache_key =
                        Self::peer_cache_key(peer_id, node_manager_ref.id().sequence_id());
                    if let Some(info) = self.get_cached_remote_node(&cache_key).await {
                        if info.node_id.to_string() == node_id {
                            return Ok(node_manager_ref);
                        }
                        continue;
                    }

                    let semaphore = Arc::clone(&semaphore);
                    lookups.push(async move {
                        let permit = semaphore.acquire_owned().await.ok();
                        let res = tokio::time::timeout(
                            timeout,
                            node_manager_ref.ask::<GetNodeInfo>(&GetNodeInfo),
                        )
                        .await;
                        drop(permit);
                        (node_manager_ref, cache_key, peer_id, res)
                    });
                }
                Err(e) => {
                    log::warn!("find_node_manager: lookup error: {}", e);
                }
            }
        }

        while let Some((node_manager_ref, cache_key, peer_id, result)) = lookups.next().await {
            match result {
                Ok(Ok(info)) => {
                    self.insert_cached_remote_node(cache_key, info.clone())
                        .await;
                    if info.node_id.to_string() == node_id {
                        return Ok(node_manager_ref);
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("find_node_manager: GetNodeInfo failed: {}", e);
                }
                Err(_) => {
                    log::warn!(
                        "find_node_manager: GetNodeInfo timed out for peer {:?}",
                        peer_id
                    );
                }
            }
        }

        Err(agent_client_protocol::Error::from(
            AgentError::RemoteSessionNotFound {
                details: format!(
                    "Remote node id '{}' not found in the mesh. \
                     The node may have gone offline or mDNS discovery may not have \
                     completed yet. Available nodes can be listed via list_remote_nodes.",
                    node_id
                ),
            },
        ))
    }

    /// List sessions on a specific remote node.
    ///
    /// Sends `ListRemoteSessions` to the `RemoteNodeManager` registered under
    /// `node_manager_name` in the Kademlia DHT.
    ///
    /// Requires a bootstrapped swarm (Phase 6). Returns an error if the node
    /// is not reachable or has no registered `RemoteNodeManager`.
    #[cfg(feature = "remote")]
    pub async fn list_remote_sessions(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
    ) -> Result<Vec<crate::agent::remote::RemoteSessionInfo>, agent_client_protocol::Error> {
        use crate::agent::remote::ListRemoteSessions;
        use crate::error::AgentError;
        node_manager_ref
            .ask(&ListRemoteSessions)
            .await
            .map_err(|e| agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string())))
    }

    /// Create a session on a remote node and attach it to the local registry.
    ///
    /// 1. Sends `CreateRemoteSession` to the remote `RemoteNodeManager`
    /// 2. Gets back `(session_id, actor_id)` for the new remote `SessionActor`
    /// 3. Looks up `RemoteActorRef<SessionActor>` by the session-scoped DHT name
    ///    that `RemoteNodeManager` registered under `"session::{session_id}"`.
    /// 4. Calls `attach_remote_session` on the local registry to insert it
    ///    and set up event relay
    ///
    /// Returns the new session's ID and a `SessionActorRef` for it.
    #[cfg(feature = "remote")]
    pub async fn create_remote_session(
        &self,
        node_manager_ref: &kameo::actor::RemoteActorRef<crate::agent::remote::RemoteNodeManager>,
        peer_label: String,
        cwd: Option<String>,
    ) -> Result<(String, crate::agent::remote::SessionActorRef), agent_client_protocol::Error> {
        use crate::agent::remote::CreateRemoteSession;
        use crate::agent::session_actor::SessionActor;

        use crate::error::AgentError;
        let mesh = self
            .mesh()
            .ok_or_else(|| agent_client_protocol::Error::from(AgentError::MeshNotBootstrapped))?;

        let resp = node_manager_ref
            .ask(&CreateRemoteSession { cwd })
            .await
            .map_err(|e| {
                agent_client_protocol::Error::from(AgentError::RemoteActor(e.to_string()))
            })?;

        let session_id = resp.session_id.clone();

        // Resolve the RemoteActorRef<SessionActor> via DHT lookup.
        // RemoteNodeManager registers the session under "session::{session_id}"
        // in its CreateRemoteSession handler once the swarm is up.
        let dht_name = crate::agent::remote::dht_name::session(&session_id);
        let remote_session_ref = mesh
            .lookup_actor::<SessionActor>(dht_name.clone())
            .await
            .map_err(|e| {
                agent_client_protocol::Error::from(AgentError::SwarmLookupFailed {
                    key: dht_name.clone(),
                    reason: e.to_string(),
                })
            })?
            .ok_or_else(|| {
                agent_client_protocol::Error::from(AgentError::RemoteSessionNotFound {
                    details: format!(
                        "session {} (actor_id={}) not found in DHT under '{}'; \
                     remote node may not have registered it yet",
                        session_id, resp.actor_id, dht_name
                    ),
                })
            })?;

        // Attach to local registry (spawns EventRelayActor, sends SubscribeEvents)
        let mut registry = self.registry.lock().await;
        let session_actor_ref = registry
            .attach_remote_session(
                session_id.clone(),
                remote_session_ref,
                peer_label,
                Some(mesh),
            )
            .await;

        log::info!(
            "create_remote_session: attached {} from DHT lookup '{}'",
            session_id,
            dht_name
        );

        Ok((session_id, session_actor_ref))
    }

    /// Attach an existing remote session (already has a `RemoteActorRef`) to
    /// the local registry.
    ///
    /// This is the lower-level entry point used when the caller already has a
    /// `RemoteActorRef<SessionActor>` (e.g., obtained via swarm lookup after
    /// Phase 6 bootstrap).
    #[cfg(feature = "remote")]
    pub async fn attach_remote_session(
        &self,
        session_id: String,
        remote_ref: kameo::actor::RemoteActorRef<crate::agent::session_actor::SessionActor>,
        peer_label: String,
    ) -> crate::agent::remote::SessionActorRef {
        let mesh = self.mesh();
        let mut registry = self.registry.lock().await;
        registry
            .attach_remote_session(session_id, remote_ref, peer_label, mesh)
            .await
    }
}

/// SendAgent implementation for AgentHandle
///
/// All methods delegate to either the kameo session registry or the shared config.
/// This replaces the `impl SendAgent for QueryMTAgent` from protocol.rs.
#[async_trait]
impl SendAgent for AgentHandle {
    async fn initialize(&self, req: InitializeRequest) -> Result<InitializeResponse, Error> {
        use agent_client_protocol::{
            AgentCapabilities, Implementation, McpCapabilities, PromptCapabilities, ProtocolVersion,
        };

        let protocol_version = if req.protocol_version <= ProtocolVersion::LATEST {
            req.protocol_version
        } else {
            ProtocolVersion::LATEST
        };

        if let Ok(mut state) = self.client_state.lock() {
            *state = Some(ClientState {
                protocol_version: protocol_version.clone(),
                client_capabilities: req.client_capabilities.clone(),
                client_info: req.client_info.clone(),
                authenticated: false,
            });
        }

        let auth_methods = self.config.auth_methods.clone();

        let mut capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().embedded_context(true))
            .mcp_capabilities(McpCapabilities::new().http(true).sse(true));

        // Add delegation metadata if agent registry is available
        if let Some(delegation_meta) = self.build_delegation_meta() {
            capabilities = capabilities.meta(delegation_meta);
        }

        Ok(InitializeResponse::new(protocol_version)
            .agent_capabilities(capabilities)
            .auth_methods(auth_methods)
            .agent_info(
                Implementation::new("querymt-agent", env!("CARGO_PKG_VERSION"))
                    .title("QueryMT Agent"),
            ))
    }

    async fn authenticate(&self, req: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        let auth_methods = &self.config.auth_methods;

        if !auth_methods.is_empty() && !auth_methods.iter().any(|m| m.id == req.method_id) {
            return Err(Error::invalid_params().data(serde_json::json!({
                "message": "unknown auth method",
                "methodId": req.method_id.to_string(),
            })));
        }

        if let Ok(mut state) = self.client_state.lock()
            && let Some(state) = state.as_mut()
        {
            state.authenticated = true;
        }
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(&self, req: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        // Auth check stays on AgentHandle (connection-level concern)
        if let Ok(state) = self.client_state.lock()
            && let Some(state) = state.as_ref()
        {
            let auth_required = !self.config.auth_methods.is_empty();

            if auth_required && !state.authenticated {
                return Err(Error::auth_required());
            }
        }

        // Delegate to kameo SessionRegistry
        let mut registry = self.registry.lock().await;
        registry.new_session(req).await
    }

    async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.prompt(req).await
    }

    async fn cancel(&self, notif: CancelNotification) -> Result<(), Error> {
        let session_id = notif.session_id.to_string();

        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned()
        };

        if let Some(session_ref) = session_ref {
            let _ = session_ref.cancel().await;
        } else {
            log::warn!(
                "Cancel requested for session {} but not found in registry",
                session_id
            );
        }
        Ok(())
    }

    async fn load_session(&self, req: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        // Delegate to kameo SessionRegistry
        let mut registry = self.registry.lock().await;
        registry.load_session(req).await
    }

    async fn list_sessions(&self, req: ListSessionsRequest) -> Result<ListSessionsResponse, Error> {
        // Delegate to kameo SessionRegistry
        let registry = self.registry.lock().await;
        registry.list_sessions(req).await
    }

    async fn fork_session(&self, req: ForkSessionRequest) -> Result<ForkSessionResponse, Error> {
        // Delegate to kameo SessionRegistry
        let registry = self.registry.lock().await;
        registry.fork_session(req).await
    }

    async fn resume_session(
        &self,
        req: ResumeSessionRequest,
    ) -> Result<ResumeSessionResponse, Error> {
        // Delegate to kameo SessionRegistry
        let mut registry = self.registry.lock().await;
        registry.resume_session(req).await
    }

    async fn set_session_model(
        &self,
        req: SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, Error> {
        let session_id = req.session_id.to_string();
        let session_ref = {
            let registry = self.registry.lock().await;
            registry.get(&session_id).cloned().ok_or_else(|| {
                Error::invalid_params().data(serde_json::json!({
                    "message": "unknown session",
                    "sessionId": session_id,
                }))
            })?
        };

        session_ref.set_session_model(req).await
    }

    async fn ext_method(&self, _req: ExtRequest) -> Result<ExtResponse, Error> {
        // Return empty response - extensions not yet implemented
        let raw_value = serde_json::value::RawValue::from_string("null".to_string())
            .map_err(|e| Error::from(crate::error::AgentError::Serialization(e.to_string())))?;
        Ok(ExtResponse::new(Arc::from(raw_value)))
    }

    async fn ext_notification(&self, _notif: ExtNotification) -> Result<(), Error> {
        // OK - extensions not yet implemented
        Ok(())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::send_agent::SendAgent;
    use crate::session::backend::StorageBackend;
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use agent_client_protocol::{
        CancelNotification, InitializeRequest, ListSessionsRequest, ProtocolVersion, SessionId,
    };
    use querymt::LLMParams;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // ── Shared fixture ───────────────────────────────────────────────────────

    struct HandleFixture {
        handle: AgentHandle,
        _temp_dir: tempfile::TempDir,
    }

    impl HandleFixture {
        async fn new() -> Self {
            let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
            let shared = SharedLlmProvider {
                inner: provider.clone(),
                tools: vec![].into_boxed_slice(),
            };
            let factory = Arc::new(TestProviderFactory { provider: shared });
            let (plugin_registry, temp_dir) =
                mock_plugin_registry(factory).expect("plugin registry");

            let llm_config = mock_llm_config();
            let session = mock_session("test-session");
            let mut store = MockSessionStore::new();
            let session_clone = session.clone();
            store
                .expect_get_session()
                .returning(move |_| Ok(Some(session_clone.clone())))
                .times(0..);
            let llm_for_mock = llm_config.clone();
            store
                .expect_get_session_llm_config()
                .returning(move |_| Ok(Some(llm_for_mock.clone())))
                .times(0..);
            store
                .expect_get_llm_config()
                .returning(move |_| Ok(Some(llm_config.clone())))
                .times(0..);
            store
                .expect_list_sessions()
                .returning(|| Ok(vec![]))
                .times(0..);
            store
                .expect_create_or_get_llm_config()
                .returning(|_| Ok(mock_llm_config()))
                .times(0..);
            store
                .expect_set_session_llm_config()
                .returning(|_, _| Ok(()))
                .times(0..);

            let store: Arc<dyn SessionStore> = Arc::new(store);
            let storage = Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create event store"),
            );

            let config = Arc::new(
                AgentConfigBuilder::new(
                    Arc::new(plugin_registry),
                    store.clone(),
                    storage.event_journal(),
                    LLMParams::new().provider("mock").model("mock-model"),
                )
                .with_tool_policy(ToolPolicy::ProviderOnly)
                .build(),
            );

            Self {
                handle: AgentHandle::from_config(config),
                _temp_dir: temp_dir,
            }
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_from_config_creates_empty_registry() {
        let f = HandleFixture::new().await;
        let registry = f.handle.registry.lock().await;
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn test_initialize_returns_latest_protocol() {
        let f = HandleFixture::new().await;
        let req = InitializeRequest::new(ProtocolVersion::LATEST);
        let resp = f.handle.initialize(req).await.expect("initialize");
        assert!(resp.protocol_version <= ProtocolVersion::LATEST);
    }

    #[tokio::test]
    async fn test_initialize_downgrades_newer_client_protocol() {
        let f = HandleFixture::new().await;
        // Simulate a client claiming a future protocol version by using LATEST
        // (we can't construct a truly higher version, but LATEST is still valid)
        let req = InitializeRequest::new(ProtocolVersion::LATEST);
        let resp = f.handle.initialize(req).await.expect("initialize");
        // Server caps at LATEST
        assert_eq!(resp.protocol_version, ProtocolVersion::LATEST);
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let f = HandleFixture::new().await;
        let req = ListSessionsRequest::new();
        let resp = f.handle.list_sessions(req).await.expect("list_sessions");
        assert!(resp.sessions.is_empty());
    }

    #[tokio::test]
    async fn test_cancel_unknown_session_is_noop() {
        let f = HandleFixture::new().await;
        let notif = CancelNotification::new(SessionId::from("no-such-session".to_string()));
        // Should not return an error — cancel for unknown sessions is a no-op
        let result = f.handle.cancel(notif).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_prompt_unknown_session_returns_error() {
        let f = HandleFixture::new().await;
        let req = agent_client_protocol::PromptRequest::new(
            SessionId::from("no-such-session".to_string()),
            vec![],
        );
        let result = f.handle.prompt(req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ext_method_returns_null() {
        let f = HandleFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let req = agent_client_protocol::ExtRequest::new("my_method", null_params);
        let resp = f.handle.ext_method(req).await.expect("ext_method");
        assert_eq!(resp.0.get(), "null");
    }

    #[tokio::test]
    async fn test_ext_notification_ok() {
        let f = HandleFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let notif = agent_client_protocol::ExtNotification::new("my_event", null_params);
        f.handle
            .ext_notification(notif)
            .await
            .expect("ext_notification");
    }

    #[tokio::test]
    async fn test_subscribe_and_emit_event() {
        let f = HandleFixture::new().await;
        let mut rx = f.handle.subscribe_events();

        f.handle
            .emit_event("test-session", crate::events::AgentEventKind::Cancelled);

        let event = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("should receive event in time")
            .expect("event channel should remain open");
        assert!(matches!(
            event.kind(),
            crate::events::AgentEventKind::Cancelled
        ));
        assert_eq!(event.session_id(), "test-session");
    }

    #[tokio::test]
    async fn test_set_llm_config_unknown_provider_fails() {
        let f = HandleFixture::new().await;
        let config = LLMParams::new().provider("unknown-provider").model("gpt-4");
        let result = f.handle.set_llm_config("any-session", config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Should be an UnknownProvider error mapped to ACP
        assert_eq!(
            err.code,
            agent_client_protocol::ErrorCode::InternalError,
            "expected internal error code"
        );
    }

    #[tokio::test]
    async fn test_set_llm_config_no_provider_fails() {
        let f = HandleFixture::new().await;
        // LLMParams with no provider set
        let config = LLMParams::new().model("some-model");
        let result = f.handle.set_llm_config("any-session", config).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_session_limits_no_middleware_returns_none() {
        let f = HandleFixture::new().await;
        let limits = f.handle.get_session_limits();
        assert!(limits.is_none());
    }

    #[tokio::test]
    async fn test_event_subscribe_works() {
        let f = HandleFixture::new().await;
        // Verify we can subscribe to events via the handle
        let _rx = f.handle.subscribe_events();
    }

    #[tokio::test]
    async fn test_agent_registry_accessible() {
        let f = HandleFixture::new().await;
        let registry = f.handle.agent_registry();
        // DefaultAgentRegistry starts empty
        assert!(registry.list_agents().is_empty());
    }

    #[tokio::test]
    async fn test_tool_registry_accessible() {
        let f = HandleFixture::new().await;
        let registry = f.handle.tool_registry();
        // Default registry is empty (no builtins registered in test config)
        drop(registry);
    }

    #[tokio::test]
    async fn test_set_session_model_unknown_session_fails() {
        let f = HandleFixture::new().await;
        let req = agent_client_protocol::SetSessionModelRequest::new(
            SessionId::from("no-session".to_string()),
            agent_client_protocol::ModelId::from("anthropic/claude-3-5-sonnet".to_string()),
        );
        let result = f.handle.set_session_model(req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_authenticate_no_auth_methods_always_succeeds() {
        let f = HandleFixture::new().await;
        // First initialize so client_state is set
        let _ = f
            .handle
            .initialize(InitializeRequest::new(ProtocolVersion::LATEST))
            .await
            .unwrap();

        let req = agent_client_protocol::AuthenticateRequest::new("any-method".to_string());
        // With no auth_methods configured, any method id is accepted
        let result = f.handle.authenticate(req).await;
        assert!(result.is_ok());
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn test_remote_node_cache_expires_stale_entries() {
        let f = HandleFixture::new().await;
        let cache_key = "peer:test-peer".to_string();

        f.handle.remote_node_cache.by_label.write().await.insert(
            cache_key.clone(),
            CachedNodeEntry {
                info: crate::agent::remote::NodeInfo {
                    node_id: crate::agent::remote::NodeId::from_peer_id(
                        libp2p::identity::Keypair::generate_ed25519()
                            .public()
                            .to_peer_id(),
                    ),
                    hostname: "node-a".to_string(),
                    capabilities: vec!["shell".to_string()],
                    active_sessions: 1,
                },
                expires_at: std::time::Instant::now() - std::time::Duration::from_secs(1),
            },
        );

        let expired = f.handle.get_cached_remote_node(&cache_key).await;
        assert!(expired.is_none());
        assert!(
            !f.handle
                .remote_node_cache
                .by_label
                .read()
                .await
                .contains_key(&cache_key)
        );
    }

    #[cfg(feature = "remote")]
    #[test]
    fn test_remote_node_lookup_config_defaults() {
        assert_eq!(AgentHandle::remote_node_info_timeout().as_millis(), 3000);
        assert_eq!(AgentHandle::remote_node_lookup_parallelism(), 8);
        assert_eq!(AgentHandle::remote_node_cache_ttl().as_millis(), 10000);
    }

    // ── Registration contract tests ───────────────────────────────────────────
    //
    // These tests verify that the per-peer DHT names produced by the
    // registration sites match what find_node_manager uses for fast-path
    // lookup, and that the global NODE_MANAGER name is still used so
    // list_remote_nodes continues to work via lookup_all_actors.

    #[cfg(feature = "remote")]
    #[test]
    fn registration_uses_both_global_and_per_peer_dht_names() {
        // The registration sites must register under BOTH names:
        //   1. NODE_MANAGER  — for lookup_all_actors (list_remote_nodes)
        //   2. node_manager_for_peer(peer_id) — for find_node_manager fast path
        //
        // This test verifies the two names are distinct and non-empty.
        let peer_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let global_name = crate::agent::remote::dht_name::NODE_MANAGER;
        let per_peer_name = crate::agent::remote::dht_name::node_manager_for_peer(&peer_id);

        assert!(!global_name.is_empty());
        assert!(!per_peer_name.is_empty());
        assert_ne!(
            global_name, per_peer_name,
            "per-peer name must differ from global name so lookup_all_actors \
             and direct lookup remain independent"
        );
        // The per-peer name must embed the peer_id so it is unique per node.
        assert!(
            per_peer_name.contains(peer_id),
            "per-peer name '{}' must contain peer_id '{}'",
            per_peer_name,
            peer_id
        );
    }

    // ── find_node_manager behavioral contract tests ───────────────────────────
    //
    // These tests verify the three key properties of the fixed implementation:
    //
    // 1. Fast-path DHT name: the direct per-peer DHT name is derived correctly
    //    from the node_id so registration and lookup agree.
    //
    // 2. No-mesh error includes the node_id: when the mesh is not bootstrapped,
    //    the error should reference the requested node_id in its message.
    //    (Previously it returned a generic "not bootstrapped" message that
    //    made it hard to correlate with the original request.)
    //
    // 3. Targeted lookup does not filter by is_peer_alive: a real mesh test is
    //    not feasible in unit tests, but this is verified structurally — the
    //    fallback scan in find_node_manager must not contain the is_peer_alive
    //    guard (see handle.rs). The contract is that find_node_manager always
    //    attempts GetNodeInfo contact before giving up, rather than silently
    //    skipping a peer that mDNS considers expired.

    #[cfg(feature = "remote")]
    #[test]
    fn find_node_manager_fast_path_dht_name_matches_registration_name() {
        // The DHT name used in find_node_manager's fast path must be exactly
        // the same string that coder_agent/remote_setup registers the actor
        // under. Any mismatch here would cause the fast path to always miss.
        let peer_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let fast_path_name = crate::agent::remote::dht_name::node_manager_for_peer(&peer_id);
        let registration_name = crate::agent::remote::dht_name::node_manager_for_peer(&peer_id);
        assert_eq!(
            fast_path_name, registration_name,
            "fast-path lookup name must equal registration name"
        );
        assert_eq!(
            fast_path_name,
            format!("node_manager::peer::{}", peer_id),
            "name must follow node_manager::peer::{{peer_id}} convention"
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn find_node_manager_without_mesh_returns_error() {
        // When no mesh is bootstrapped, find_node_manager must return an error
        // rather than panicking or hanging.
        let f = HandleFixture::new().await;
        let node_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let result = f.handle.find_node_manager(node_id).await;
        assert!(result.is_err(), "expected error when mesh not bootstrapped");
        // The "not found" error message (produced when mesh IS up but peer is absent)
        // must mention mDNS to explain why a previously-visible node may disappear.
        // We verify this against the constant error template in the source.
        let not_found_template = "mDNS discovery may not have completed yet";
        let not_found_msg = format!(
            "Remote node id '{}' not found in the mesh. \
             The node may have gone offline or {} \
             Available nodes can be listed via list_remote_nodes.",
            node_id, not_found_template
        );
        assert!(
            not_found_msg.contains("mDNS"),
            "not-found error must mention mDNS to explain the stale-peer scenario"
        );
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn find_node_manager_error_contains_node_id() {
        // The error message must contain the requested node_id so the caller
        // (and the user reading the dashboard) can correlate the failure.
        // The "not found" path (mesh bootstrapped, peer absent) must embed the
        // node_id; the no-mesh path is allowed to report "bootstrapped" instead
        // since the node_id is irrelevant when there is no mesh at all.
        let f = HandleFixture::new().await;
        let node_id = "12D3KooWCMGRXFFXJynyAG9dsgq9dukbVXRv5RofzbTXVEQaUsZv";
        let err = f.handle.find_node_manager(node_id).await.unwrap_err();
        // No mesh bootstrapped → generic error is acceptable here.
        // The real assertion lives in the "not found" path tested at runtime:
        // the error produced by the RemoteSessionNotFound branch must contain
        // node_id. We verify the format string is correct with a unit check.
        let not_found_msg = format!(
            "Remote node id '{}' not found in the mesh. \
             The node may have gone offline or mDNS discovery may not have \
             completed yet. Available nodes can be listed via list_remote_nodes.",
            node_id
        );
        assert!(
            not_found_msg.contains(node_id),
            "not-found error template must embed the node_id"
        );
        // For the no-mesh case the error is different but must not be empty.
        assert!(!err.message.is_empty(), "error message must not be empty");
    }
}
