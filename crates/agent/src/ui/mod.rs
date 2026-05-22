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
use tokio::sync::Mutex;
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
}

impl RemoteNodeCache {
    /// Maximum age before a cached result is considered stale.
    const TTL_SECS: u64 = 5;

    fn new(nodes: Vec<crate::agent::remote::NodeInfo>) -> Self {
        Self {
            nodes,
            refreshed_at: Instant::now(),
        }
    }

    fn is_fresh(&self) -> bool {
        self.refreshed_at.elapsed().as_secs() < Self::TTL_SECS
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
    pub session_agents: Arc<Mutex<HashMap<String, String>>>,
    pub session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    pub workspace_manager: ActorRef<WorkspaceIndexManagerActor>,
    pub oauth_service: OAuthService,
    /// Cache for remote node DHT discovery results with a short TTL.
    #[cfg(feature = "remote")]
    pub remote_node_cache: Arc<Mutex<Option<RemoteNodeCache>>>,
}

impl ServerState {
    /// Return cached remote nodes if fresh, otherwise query the DHT and cache.
    ///
    /// The cache has a 5-second TTL.  Peer events invalidate it immediately
    /// via [`Self::invalidate_remote_node_cache`].
    #[cfg(feature = "remote")]
    pub async fn get_remote_nodes_cached(&self) -> Vec<crate::agent::remote::NodeInfo> {
        // Fast path: return cached result if still fresh.
        {
            let guard = self.remote_node_cache.lock().await;
            if let Some(ref cache) = *guard
                && cache.is_fresh()
            {
                return cache.nodes.clone();
            }
        }

        // Slow path: query DHT and update cache.
        let nodes = self.agent.list_remote_nodes().await;
        let mut guard = self.remote_node_cache.lock().await;
        *guard = Some(RemoteNodeCache::new(nodes.clone()));
        nodes
    }

    /// Invalidate the remote-node cache (called on peer topology changes).
    #[cfg(feature = "remote")]
    pub async fn invalidate_remote_node_cache(&self) {
        let mut guard = self.remote_node_cache.lock().await;
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
            session_agents: self.session_agents,
            session_cwds: self.session_cwds,
            workspace_manager: self.workspace_manager,
            oauth_service: self.oauth_service,
            #[cfg(feature = "remote")]
            remote_node_cache: Arc::new(Mutex::new(None)),
        };

        Router::new()
            .route("/ws", get(websocket_handler))
            .with_state(state)
    }
}

/// WebSocket upgrade handler.
async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| connection::handle_websocket_connection(socket, state))
}
