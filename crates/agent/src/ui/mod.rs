//! UI WebSocket server for the agent.
//!
//! Provides a WebSocket-based interface for UI clients to interact with
//! the agent, including session management, prompt sending, and event streaming.
//!
//! # Module Structure
//!
//! - [`messages`]: Wire protocol types (client/server messages, DTOs)
//! - [`handlers`]: Message handlers for client requests  
//! - [`session`]: Session lifecycle and routing mode management
//! - [`connection`]: WebSocket handling and event forwarding
//! - [`mentions`]: @ mention expansion for file/directory references

mod connection;
mod error;
mod handlers;
mod mentions;
mod messages;
mod session;

pub use messages::{
    RoutingMode, SessionLoadSnapshot, StreamCursor, UiAgentInfo, cursor_from_events,
};

#[cfg(test)]
mod fork_tests;
#[cfg(all(feature = "api", feature = "remote"))]
mod session_ops_remote_title_tests;
#[cfg(test)]
mod session_ops_tests;
#[cfg(test)]
mod session_stream_tests;
#[cfg(test)]
mod undo_handler_tests;

use crate::auth::service::OAuthService;
use crate::event_fanout::EventFanout;
use crate::index::WorkspaceIndexManagerActor;
use crate::profiles::{ProfileCatalog, ProfileRuntimeManager};
use crate::session::projection::ViewStore;
use crate::session::store::SessionStore;
use crate::ui::messages::UiServerMessage;
use axum::{
    Router,
    extract::{State, ws::WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use kameo::actor::ActorRef;
use messages::RoutingMode as MsgRoutingMode;
use session::collect_event_sources;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;

/// UI WebSocket server.
pub struct UiServer {
    agent: Arc<crate::agent::LocalAgentHandle>,
    view_store: Arc<dyn ViewStore>,
    session_store: Arc<dyn SessionStore>,
    default_cwd: Option<PathBuf>,
    event_sources: Vec<Arc<EventFanout>>,
    profiles: Option<Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>>,
    connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    connection_senders: Arc<Mutex<HashMap<String, mpsc::Sender<String>>>>,
    session_agents: Arc<Mutex<HashMap<String, String>>>,
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    workspace_manager: ActorRef<WorkspaceIndexManagerActor>,
    oauth_service: OAuthService,
}

/// Cached result of a remote-node DHT discovery query.
///
/// Stored in `ServerState` so concurrent callers (list_remote_nodes,
/// peer event watcher, remote session merge) can reuse a recent result
/// instead of each hitting the DHT independently.
pub(crate) struct RemoteNodeCache {
    /// Snapshot of `list_remote_nodes()` results.
    nodes: Vec<crate::agent::remote::NodeInfo>,
    /// `Instant::now()` when `nodes` was populated.
    refreshed_at: Instant,
    /// True while a caller is refreshing `nodes` from the mesh.
    refreshing: bool,
    /// Wakes waiters when the active refresh completes.
    refresh_notify: Arc<Notify>,
}

impl RemoteNodeCache {
    /// Maximum age before a cached result is considered stale.
    const TTL_SECS: u64 = 5;

    fn new(nodes: Vec<crate::agent::remote::NodeInfo>) -> Self {
        Self {
            nodes,
            refreshed_at: Instant::now(),
            refreshing: false,
            refresh_notify: Arc::new(Notify::new()),
        }
    }

    fn is_fresh(&self) -> bool {
        self.refreshed_at.elapsed().as_secs() < Self::TTL_SECS
    }

    fn begin_refresh(&mut self) -> Arc<Notify> {
        self.refreshing = true;
        self.refresh_notify.clone()
    }

    fn finish_refresh(&mut self, nodes: Vec<crate::agent::remote::NodeInfo>) {
        self.nodes = nodes;
        self.refreshed_at = Instant::now();
        self.refreshing = false;
        self.refresh_notify.notify_waiters();
    }

    fn finish_refresh_without_update(&mut self) {
        self.refreshing = false;
        self.refresh_notify.notify_waiters();
    }
}

/// Shared server state for request handlers.
#[derive(Clone)]
pub(crate) struct ServerState {
    pub agent: Arc<crate::agent::LocalAgentHandle>,
    pub view_store: Arc<dyn ViewStore>,
    pub session_store: Arc<dyn SessionStore>,
    pub default_cwd: Option<PathBuf>,
    pub event_sources: Vec<Arc<EventFanout>>,
    pub profiles: Option<Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>>,
    pub connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    /// Registered senders for broadcasting generic UI server messages to all
    /// connected clients. Inserted on WebSocket connect, removed on disconnect.
    pub connection_senders: Arc<Mutex<HashMap<String, mpsc::Sender<String>>>>,
    pub session_agents: Arc<Mutex<HashMap<String, String>>>,
    pub session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    pub workspace_manager: ActorRef<WorkspaceIndexManagerActor>,
    pub oauth_service: OAuthService,
    /// Cache for remote node DHT discovery results with a short TTL.
    #[cfg(feature = "remote")]
    pub remote_node_cache: Arc<Mutex<Option<RemoteNodeCache>>>,
}

impl ServerState {
    /// Broadcast a serialized UI message to every connected WebSocket client.
    ///
    /// Closed senders are removed opportunistically.
    pub async fn broadcast_message(&self, message: UiServerMessage) {
        let payload = match serde_json::to_string(&message) {
            Ok(json) => json,
            Err(err) => {
                log::error!(
                    "broadcast_message: failed to serialize {}: {}",
                    message.type_name(),
                    err
                );
                return;
            }
        };

        let mut senders = self.connection_senders.lock().await;
        senders.retain(|_conn_id, tx| !tx.is_closed());
        for tx in senders.values() {
            let _ = tx.try_send(payload.clone());
        }
    }

    /// Send the current model inventory snapshot to all connected UI clients.
    pub async fn broadcast_model_snapshot(&self) {
        let models = self.agent.model_inventory.get_all_models().await;
        self.broadcast_message(UiServerMessage::AllModelsList { models })
            .await;
    }
}

impl ServerState {
    /// Return cached remote nodes if fresh, otherwise query the DHT and cache.
    ///
    /// The cache has a 5-second TTL.  Peer events invalidate it immediately
    /// via [`Self::invalidate_remote_node_cache`].
    #[cfg(feature = "remote")]
    pub async fn get_remote_nodes_cached(&self) -> Vec<crate::agent::remote::NodeInfo> {
        loop {
            let (refresh_notify, should_refresh) = {
                let mut guard = self.remote_node_cache.lock().await;
                match guard.as_mut() {
                    Some(cache) if cache.is_fresh() => return cache.nodes.clone(),
                    Some(cache) if cache.refreshing => (cache.refresh_notify.clone(), false),
                    Some(cache) => (cache.begin_refresh(), true),
                    None => {
                        let mut cache = RemoteNodeCache::new(Vec::new());
                        let notify = cache.begin_refresh();
                        *guard = Some(cache);
                        (notify, true)
                    }
                }
            };

            if should_refresh {
                let nodes = self.agent.list_remote_nodes().await;
                let mut guard = self.remote_node_cache.lock().await;
                if let Some(cache) = guard.as_mut() {
                    cache.finish_refresh(nodes.clone());
                } else {
                    *guard = Some(RemoteNodeCache::new(nodes.clone()));
                }
                return nodes;
            }

            refresh_notify.notified().await;
        }
    }

    /// Invalidate the remote-node cache (called on peer topology changes).
    #[cfg(feature = "remote")]
    pub async fn invalidate_remote_node_cache(&self) {
        let mut guard = self.remote_node_cache.lock().await;
        if let Some(cache) = guard.as_mut()
            && cache.refreshing
        {
            cache.finish_refresh_without_update();
        }
        *guard = None;
    }
}

/// State for a single WebSocket connection.
#[derive(Debug)]
pub(crate) struct ConnectionState {
    pub routing_mode: MsgRoutingMode,
    pub active_agent_id: String,
    pub sessions: HashMap<String, String>,
    pub subscribed_sessions: HashSet<String>,
    pub session_cursors: HashMap<String, StreamCursor>,
    pub current_workspace_root: Option<PathBuf>,
    pub file_index_forwarder: Option<JoinHandle<()>>,
}

impl UiServer {
    /// Create a new UI server.
    pub fn new(
        agent: Arc<crate::agent::LocalAgentHandle>,
        view_store: Arc<dyn ViewStore>,
        session_store: Arc<dyn SessionStore>,
        default_cwd: Option<PathBuf>,
    ) -> Self {
        Self::build(agent, view_store, session_store, default_cwd, None)
    }

    /// Create a UI server backed by a profile runtime manager.
    pub fn with_profiles(
        agent: Arc<crate::agent::LocalAgentHandle>,
        view_store: Arc<dyn ViewStore>,
        session_store: Arc<dyn SessionStore>,
        default_cwd: Option<PathBuf>,
        profiles: Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>,
    ) -> Self {
        Self::build(
            agent,
            view_store,
            session_store,
            default_cwd,
            Some(profiles),
        )
    }

    fn build(
        agent: Arc<crate::agent::LocalAgentHandle>,
        view_store: Arc<dyn ViewStore>,
        session_store: Arc<dyn SessionStore>,
        default_cwd: Option<PathBuf>,
        profiles: Option<Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>>,
    ) -> Self {
        let event_sources = collect_event_sources(&agent);

        Self {
            agent: agent.clone(),
            view_store,
            session_store,
            default_cwd: default_cwd.or_else(|| std::env::current_dir().ok()),
            event_sources,
            profiles,
            connections: Arc::new(Mutex::new(HashMap::new())),
            connection_senders: Arc::new(Mutex::new(HashMap::new())),
            session_agents: Arc::new(Mutex::new(HashMap::new())),
            session_cwds: Arc::new(Mutex::new(HashMap::new())),
            workspace_manager: agent.workspace_manager_actor(),
            oauth_service: agent.oauth_service.clone(),
        }
    }

    /// Build the router for the UI server.
    pub fn router(self) -> Router {
        let state = ServerState {
            agent: self.agent,
            view_store: self.view_store,
            session_store: self.session_store,
            default_cwd: self.default_cwd,
            event_sources: self.event_sources,
            profiles: self.profiles,
            connections: self.connections,
            connection_senders: self.connection_senders,
            session_agents: self.session_agents,
            session_cwds: self.session_cwds,
            workspace_manager: self.workspace_manager,
            oauth_service: self.oauth_service,
            #[cfg(feature = "remote")]
            remote_node_cache: Arc::new(Mutex::new(None)),
        };

        let router = Router::new()
            .route("/ws", get(websocket_handler))
            .with_state(state.clone());

        // Spawn a single model-inventory broadcast loop so all connected UI
        // clients receive updated model lists automatically.
        let broadcast_state = state;
        tokio::spawn(async move {
            let mut rx = broadcast_state.agent.model_inventory.subscribe_updates();
            // Drop the pending initial value so we only react to future refreshes.
            let _ = rx.try_recv();
            loop {
                match rx.recv().await {
                    Ok(_version) => {
                        broadcast_state.broadcast_model_snapshot().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        broadcast_state.broadcast_model_snapshot().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        router
    }
}

/// WebSocket upgrade handler.
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| connection::handle_websocket_connection(socket, state))
}
