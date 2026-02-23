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

#[cfg(test)]
mod session_stream_tests;
#[cfg(test)]
mod undo_handler_tests;

use crate::event_fanout::EventFanout;
use crate::index::WorkspaceIndexManagerActor;
use crate::session::projection::ViewStore;
use crate::session::store::SessionStore;
use crate::ui::messages::StreamCursor;
use axum::{
    Router,
    extract::{State, ws::WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
};
use kameo::actor::ActorRef;
use messages::{ModelEntry, RoutingMode as MsgRoutingMode};
use moka::future::Cache;
use session::collect_event_sources;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// TTL for model cache (30 minutes)
const MODEL_CACHE_TTL: Duration = Duration::from_secs(30 * 60);

/// UI WebSocket server.
pub struct UiServer {
    agent: Arc<crate::agent::AgentHandle>,
    view_store: Arc<dyn ViewStore>,
    session_store: Arc<dyn SessionStore>,
    default_cwd: Option<PathBuf>,
    event_sources: Vec<Arc<EventFanout>>,
    connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    session_agents: Arc<Mutex<HashMap<String, String>>>,
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    workspace_manager: ActorRef<WorkspaceIndexManagerActor>,
    model_cache: Cache<(), Vec<ModelEntry>>,
    oauth_flows: Arc<Mutex<HashMap<String, PendingOAuthFlow>>>,
    oauth_callback_listener: Arc<Mutex<Option<ActiveOAuthCallbackListener>>>,
}

/// Shared server state for request handlers.
#[derive(Clone)]
pub(crate) struct ServerState {
    pub agent: Arc<crate::agent::AgentHandle>,
    pub view_store: Arc<dyn ViewStore>,
    pub session_store: Arc<dyn SessionStore>,
    pub default_cwd: Option<PathBuf>,
    pub event_sources: Vec<Arc<EventFanout>>,
    pub connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    pub session_agents: Arc<Mutex<HashMap<String, String>>>,
    pub session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    pub workspace_manager: ActorRef<WorkspaceIndexManagerActor>,
    pub model_cache: Cache<(), Vec<ModelEntry>>,
    pub oauth_flows: Arc<Mutex<HashMap<String, PendingOAuthFlow>>>,
    pub oauth_callback_listener: Arc<Mutex<Option<ActiveOAuthCallbackListener>>>,
}

pub(crate) struct ActiveOAuthCallbackListener {
    pub flow_id: String,
    pub conn_id: String,
    pub provider: String,
    pub stop_tx: tokio::sync::oneshot::Sender<()>,
    pub task: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingOAuthFlow {
    pub conn_id: String,
    pub provider: String,
    pub state: String,
    pub verifier: String,
    pub created_at: Instant,
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

pub(crate) fn cursor_from_events(events: &[crate::events::AgentEvent]) -> StreamCursor {
    let mut cursor = StreamCursor::default();

    for event in events {
        match event.origin {
            crate::events::EventOrigin::Local => {
                cursor.local_seq = cursor.local_seq.max(event.seq);
            }
            crate::events::EventOrigin::Remote => {
                if let Some(source) = event.source_node.as_ref() {
                    cursor
                        .remote_seq_by_source
                        .entry(source.clone())
                        .and_modify(|seq| *seq = (*seq).max(event.seq))
                        .or_insert(event.seq);
                }
            }
            crate::events::EventOrigin::Unknown(_) => {
                cursor.local_seq = cursor.local_seq.max(event.seq);
            }
        }
    }

    cursor
}

impl UiServer {
    /// Create a new UI server.
    pub fn new(
        agent: Arc<crate::agent::AgentHandle>,
        view_store: Arc<dyn ViewStore>,
        session_store: Arc<dyn SessionStore>,
        default_cwd: Option<PathBuf>,
    ) -> Self {
        let event_sources = collect_event_sources(&agent);
        let model_cache = Cache::builder().time_to_live(MODEL_CACHE_TTL).build();

        Self {
            agent: agent.clone(),
            view_store,
            session_store,
            default_cwd: default_cwd.or_else(|| std::env::current_dir().ok()),
            event_sources,
            connections: Arc::new(Mutex::new(HashMap::new())),
            session_agents: Arc::new(Mutex::new(HashMap::new())),
            session_cwds: Arc::new(Mutex::new(HashMap::new())),
            workspace_manager: agent.workspace_manager_actor(),
            model_cache,
            oauth_flows: Arc::new(Mutex::new(HashMap::new())),
            oauth_callback_listener: Arc::new(Mutex::new(None)),
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
            connections: self.connections,
            session_agents: self.session_agents,
            session_cwds: self.session_cwds,
            workspace_manager: self.workspace_manager,
            model_cache: self.model_cache,
            oauth_flows: self.oauth_flows,
            oauth_callback_listener: self.oauth_callback_listener,
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
