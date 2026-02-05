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
mod handlers;
mod mentions;
mod messages;
mod session;

pub use messages::{RoutingMode, UiAgentInfo};

use crate::agent::QueryMTAgent;
use crate::event_bus::EventBus;
use crate::index::WorkspaceIndexManager;
use crate::session::projection::ViewStore;
use axum::{
    Router,
    extract::{State, ws::WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use messages::{ModelEntry, RoutingMode as MsgRoutingMode};
use moka::future::Cache;
use session::collect_event_sources;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// TTL for model cache (30 minutes)
const MODEL_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// UI WebSocket server.
pub struct UiServer {
    agent: Arc<QueryMTAgent>,
    view_store: Arc<dyn ViewStore>,
    event_sources: Vec<Arc<EventBus>>,
    connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    session_agents: Arc<Mutex<HashMap<String, String>>>,
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    workspace_manager: Arc<WorkspaceIndexManager>,
    model_cache: Cache<(), Vec<ModelEntry>>,
}

/// Shared server state for request handlers.
#[derive(Clone)]
pub(crate) struct ServerState {
    pub agent: Arc<QueryMTAgent>,
    pub view_store: Arc<dyn ViewStore>,
    pub event_sources: Vec<Arc<EventBus>>,
    pub connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    pub session_agents: Arc<Mutex<HashMap<String, String>>>,
    pub session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    pub workspace_manager: Arc<WorkspaceIndexManager>,
    pub model_cache: Cache<(), Vec<ModelEntry>>,
}

/// State for a single WebSocket connection.
#[derive(Debug, Clone)]
pub(crate) struct ConnectionState {
    pub routing_mode: MsgRoutingMode,
    pub active_agent_id: String,
    pub sessions: HashMap<String, String>,
    pub subscribed_sessions: HashSet<String>,
    pub current_workspace_root: Option<PathBuf>,
}

impl UiServer {
    /// Create a new UI server.
    pub fn new(agent: Arc<QueryMTAgent>, view_store: Arc<dyn ViewStore>) -> Self {
        let event_sources = collect_event_sources(&agent);
        let model_cache = Cache::builder().time_to_live(MODEL_CACHE_TTL).build();

        Self {
            agent: agent.clone(),
            view_store,
            event_sources,
            connections: Arc::new(Mutex::new(HashMap::new())),
            session_agents: Arc::new(Mutex::new(HashMap::new())),
            session_cwds: Arc::new(Mutex::new(HashMap::new())),
            workspace_manager: agent.workspace_index_manager(),
            model_cache,
        }
    }

    /// Build the router for the UI server.
    pub fn router(self) -> Router {
        let state = ServerState {
            agent: self.agent,
            view_store: self.view_store,
            event_sources: self.event_sources,
            connections: self.connections,
            session_agents: self.session_agents,
            session_cwds: self.session_cwds,
            workspace_manager: self.workspace_manager,
            model_cache: self.model_cache,
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
