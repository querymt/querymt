use crate::agent::QueryMTAgent;
use crate::event_bus::EventBus;
use crate::events::{AgentEvent, AgentEventKind};
use crate::send_agent::SendAgent;
use crate::session::domain::ForkOrigin;
use crate::session::projection::{AuditView, ViewStore};
use agent_client_protocol::{NewSessionRequest, PromptRequest};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use futures_util::{sink::SinkExt, stream::StreamExt as FuturesStreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use uuid::Uuid;

const PRIMARY_AGENT_ID: &str = "primary";

#[derive(Debug, Clone, Serialize)]
pub struct UiAgentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    Single,
    Broadcast,
}

pub struct UiServer {
    agent: Arc<QueryMTAgent>,
    view_store: Arc<dyn ViewStore>,
    event_sources: Vec<Arc<EventBus>>,
    connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    session_owners: Arc<Mutex<HashMap<String, String>>>,
    session_agents: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Clone)]
struct ServerState {
    agent: Arc<QueryMTAgent>,
    view_store: Arc<dyn ViewStore>,
    event_sources: Vec<Arc<EventBus>>,
    connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    session_owners: Arc<Mutex<HashMap<String, String>>>,
    session_agents: Arc<Mutex<HashMap<String, String>>>,
}

#[derive(Debug, Clone)]
struct ConnectionState {
    routing_mode: RoutingMode,
    active_agent_id: String,
    sessions: HashMap<String, String>,
}

impl UiServer {
    pub fn new(agent: Arc<QueryMTAgent>, view_store: Arc<dyn ViewStore>) -> Self {
        let event_sources = collect_event_sources(&agent);
        Self {
            agent,
            view_store,
            event_sources,
            connections: Arc::new(Mutex::new(HashMap::new())),
            session_owners: Arc::new(Mutex::new(HashMap::new())),
            session_agents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn router(self) -> Router {
        let state = ServerState {
            agent: self.agent,
            view_store: self.view_store,
            event_sources: self.event_sources,
            connections: self.connections,
            session_owners: self.session_owners,
            session_agents: self.session_agents,
        };

        Router::new()
            .route("/ws", get(websocket_handler))
            .with_state(state)
    }
}

fn collect_event_sources(agent: &Arc<QueryMTAgent>) -> Vec<Arc<EventBus>> {
    let mut sources = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let primary = agent.event_bus();
    if seen.insert(Arc::as_ptr(&primary) as usize) {
        sources.push(primary);
    }

    let registry = agent.agent_registry();
    for info in registry.list_agents() {
        if let Some(instance) = registry.get_agent_instance(&info.id)
            && let Some(bus) = instance
                .as_any()
                .downcast_ref::<QueryMTAgent>()
                .map(|agent| agent.event_bus())
            && seen.insert(Arc::as_ptr(&bus) as usize)
        {
            sources.push(bus);
        }
    }

    sources
}

#[derive(Serialize)]
struct SessionSummary {
    session_id: String,
    name: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UiClientMessage {
    Init,
    SetActiveAgent { agent_id: String },
    SetRoutingMode { mode: RoutingMode },
    NewSession { cwd: Option<String> },
    Prompt { text: String },
    ListSessions,
    LoadSession { session_id: String },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UiServerMessage {
    State {
        routing_mode: RoutingMode,
        active_agent_id: String,
        active_session_id: Option<String>,
        agents: Vec<UiAgentInfo>,
    },
    SessionCreated {
        agent_id: String,
        session_id: String,
    },
    Event {
        agent_id: String,
        event: AgentEvent,
    },
    Error {
        message: String,
    },
    SessionList {
        sessions: Vec<SessionSummary>,
    },
    SessionLoaded {
        session_id: String,
        audit: AuditView,
    },
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_websocket_connection(socket, state))
}

async fn handle_websocket_connection(socket: WebSocket, state: ServerState) {
    let conn_id = Uuid::new_v4().to_string();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(100);

    {
        let mut connections = state.connections.lock().await;
        connections.insert(
            conn_id.clone(),
            ConnectionState {
                routing_mode: RoutingMode::Single,
                active_agent_id: PRIMARY_AGENT_ID.to_string(),
                sessions: HashMap::new(),
            },
        );
    }

    spawn_event_forwarders(state.clone(), conn_id.clone(), tx.clone());
    send_state(&state, &conn_id, &tx).await;

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let state_for_receive = state.clone();
    let conn_id_for_receive = conn_id.clone();
    let tx_for_receive = tx.clone();
    let receive_task = tokio::spawn(async move {
        while let Some(result) = FuturesStreamExt::next(&mut ws_receiver).await {
            match result {
                Ok(Message::Text(text)) => {
                    let msg = match serde_json::from_str::<UiClientMessage>(&text) {
                        Ok(msg) => msg,
                        Err(e) => {
                            let _ = send_error(&tx_for_receive, format!("Invalid message: {}", e))
                                .await;
                            continue;
                        }
                    };
                    handle_ui_message(
                        &state_for_receive,
                        &conn_id_for_receive,
                        &tx_for_receive,
                        msg,
                    )
                    .await;
                }
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_)) => {}
                Ok(_) => {}
                Err(e) => {
                    log::error!("UI WebSocket error: {}", e);
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = receive_task => {},
    }

    let mut connections = state.connections.lock().await;
    connections.remove(&conn_id);
}

async fn handle_ui_message(
    state: &ServerState,
    conn_id: &str,
    tx: &mpsc::Sender<String>,
    msg: UiClientMessage,
) {
    match msg {
        UiClientMessage::Init => {
            send_state(state, conn_id, tx).await;
            handle_list_sessions(state, tx).await;
        }
        UiClientMessage::SetActiveAgent { agent_id } => {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.active_agent_id = agent_id;
            }
            drop(connections);
            send_state(state, conn_id, tx).await;
        }
        UiClientMessage::SetRoutingMode { mode } => {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.routing_mode = mode;
            }
            drop(connections);
            send_state(state, conn_id, tx).await;
        }
        UiClientMessage::NewSession { cwd } => {
            let cwd = resolve_cwd(cwd);

            // Clear existing sessions for this connection to start fresh
            {
                let mut connections = state.connections.lock().await;
                if let Some(conn) = connections.get_mut(conn_id) {
                    let session_ids: Vec<String> = conn.sessions.values().cloned().collect();
                    conn.sessions.clear();

                    drop(connections);

                    // Clean up ownership maps
                    let mut owners = state.session_owners.lock().await;
                    let mut agents = state.session_agents.lock().await;
                    for sid in session_ids {
                        owners.remove(&sid);
                        agents.remove(&sid);
                    }
                }
            }

            if let Err(err) = ensure_sessions_for_mode(state, conn_id, cwd.as_ref(), tx).await {
                let _ = send_error(tx, err).await;
            }

            // Auto-refresh session list after creating new session
            handle_list_sessions(state, tx).await;
        }
        UiClientMessage::Prompt { text } => {
            if text.trim().is_empty() {
                return;
            }
            let cwd = resolve_cwd(None);
            if let Err(err) = prompt_for_mode(state, conn_id, &text, cwd.as_ref(), tx).await {
                let _ = send_error(tx, err).await;
            }
        }
        UiClientMessage::ListSessions => {
            handle_list_sessions(state, tx).await;
        }
        UiClientMessage::LoadSession { session_id } => {
            handle_load_session(state, conn_id, &session_id, tx).await;
        }
    }
}

async fn ensure_sessions_for_mode(
    state: &ServerState,
    conn_id: &str,
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    let mode = current_mode(state, conn_id).await?;
    match mode {
        RoutingMode::Single => {
            let agent_id = current_active_agent(state, conn_id).await?;
            ensure_session(state, conn_id, &agent_id, cwd, tx).await?;
        }
        RoutingMode::Broadcast => {
            let agent_ids = list_agent_ids(state);
            for agent_id in agent_ids {
                ensure_session(state, conn_id, &agent_id, cwd, tx).await?;
            }
        }
    }
    Ok(())
}

async fn prompt_for_mode(
    state: &ServerState,
    conn_id: &str,
    text: &str,
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    let mode = current_mode(state, conn_id).await?;
    match mode {
        RoutingMode::Single => {
            let agent_id = current_active_agent(state, conn_id).await?;
            let session_id = ensure_session(state, conn_id, &agent_id, cwd, tx).await?;
            let agent = agent_for_id(state, &agent_id)
                .ok_or_else(|| format!("Unknown agent: {}", agent_id))?;
            send_prompt(agent, session_id, text.to_string()).await?;
        }
        RoutingMode::Broadcast => {
            let agent_ids = list_agent_ids(state);
            for agent_id in agent_ids {
                let session_id = ensure_session(state, conn_id, &agent_id, cwd, tx).await?;
                let agent = agent_for_id(state, &agent_id)
                    .ok_or_else(|| format!("Unknown agent: {}", agent_id))?;
                send_prompt(agent, session_id, text.to_string()).await?;
            }
        }
    }
    Ok(())
}

async fn send_prompt(
    agent: Arc<dyn SendAgent>,
    session_id: String,
    text: String,
) -> Result<(), String> {
    let request = PromptRequest::new(
        session_id,
        vec![agent_client_protocol::ContentBlock::Text(
            agent_client_protocol::TextContent::new(text),
        )],
    );
    agent
        .prompt(request)
        .await
        .map_err(|err| err.message)
        .map(|_| ())
}

async fn ensure_session(
    state: &ServerState,
    conn_id: &str,
    agent_id: &str,
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
) -> Result<String, String> {
    let existing = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(agent_id).cloned())
    };
    if let Some(session_id) = existing {
        return Ok(session_id);
    }

    let agent =
        agent_for_id(state, agent_id).ok_or_else(|| format!("Unknown agent: {}", agent_id))?;

    // Use empty PathBuf as sentinel for "no cwd" to work with ACP protocol
    let cwd_for_request = cwd.cloned().unwrap_or_else(PathBuf::new);
    let response = agent
        .new_session(NewSessionRequest::new(cwd_for_request))
        .await
        .map_err(|err| err.message)?;
    let session_id = response.session_id.to_string();

    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.to_string(), session_id.clone());
        }
    }

    {
        let mut owners = state.session_owners.lock().await;
        owners.insert(session_id.clone(), conn_id.to_string());
    }

    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.clone(), agent_id.to_string());
    }

    let _ = send_message(
        tx,
        UiServerMessage::SessionCreated {
            agent_id: agent_id.to_string(),
            session_id: session_id.clone(),
        },
    )
    .await;

    send_state(state, conn_id, tx).await;

    Ok(session_id)
}

fn agent_for_id(state: &ServerState, agent_id: &str) -> Option<Arc<dyn SendAgent>> {
    if agent_id == PRIMARY_AGENT_ID {
        return Some(state.agent.clone());
    }
    let registry = state.agent.agent_registry();
    registry.get_agent_instance(agent_id)
}

async fn current_active_agent(state: &ServerState, conn_id: &str) -> Result<String, String> {
    let connections = state.connections.lock().await;
    connections
        .get(conn_id)
        .map(|conn| conn.active_agent_id.clone())
        .ok_or_else(|| "Connection state missing".to_string())
}

async fn current_mode(state: &ServerState, conn_id: &str) -> Result<RoutingMode, String> {
    let connections = state.connections.lock().await;
    connections
        .get(conn_id)
        .map(|conn| conn.routing_mode)
        .ok_or_else(|| "Connection state missing".to_string())
}

fn list_agent_ids(state: &ServerState) -> Vec<String> {
    let registry = state.agent.agent_registry();
    let mut ids = vec![PRIMARY_AGENT_ID.to_string()];
    ids.extend(registry.list_agents().into_iter().map(|info| info.id));
    ids
}

fn build_agent_list(state: &ServerState) -> Vec<UiAgentInfo> {
    let mut agents = Vec::new();
    agents.push(UiAgentInfo {
        id: PRIMARY_AGENT_ID.to_string(),
        name: "Primary Agent".to_string(),
        description: "Main agent for the current session.".to_string(),
        capabilities: Vec::new(),
    });
    let registry = state.agent.agent_registry();
    for info in registry.list_agents() {
        agents.push(UiAgentInfo {
            id: info.id,
            name: info.name,
            description: info.description,
            capabilities: info.capabilities,
        });
    }
    agents
}

async fn send_state(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    let (routing_mode, active_agent_id, active_session_id) = {
        let connections = state.connections.lock().await;
        if let Some(conn) = connections.get(conn_id) {
            (
                conn.routing_mode,
                conn.active_agent_id.clone(),
                conn.sessions.get(&conn.active_agent_id).cloned(),
            )
        } else {
            (RoutingMode::Single, PRIMARY_AGENT_ID.to_string(), None)
        }
    };

    let agents = build_agent_list(state);
    let _ = send_message(
        tx,
        UiServerMessage::State {
            routing_mode,
            active_agent_id,
            active_session_id,
            agents,
        },
    )
    .await;
}

fn resolve_cwd(cwd: Option<String>) -> Option<PathBuf> {
    cwd.map(PathBuf::from)
}

async fn handle_list_sessions(state: &ServerState, tx: &mpsc::Sender<String>) {
    let sessions = match state.agent.provider.history_store().list_sessions().await {
        Ok(sessions) => sessions,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list sessions: {}", e)).await;
            return;
        }
    };

    let summaries: Vec<SessionSummary> = sessions
        .into_iter()
        .take(50) // Limit to 50 most recent
        .map(|s| SessionSummary {
            session_id: s.public_id,
            name: s.name,
            created_at: s.created_at.and_then(|t| t.format(&Rfc3339).ok()),
            updated_at: s.updated_at.and_then(|t| t.format(&Rfc3339).ok()),
        })
        .collect();

    let _ = send_message(
        tx,
        UiServerMessage::SessionList {
            sessions: summaries,
        },
    )
    .await;
}

async fn handle_load_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    // 1. Get full audit view (includes events, tasks, decisions, artifacts, etc.)
    let audit = match state.view_store.get_audit_view(session_id).await {
        Ok(audit) => audit,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to load session: {}", e)).await;
            return;
        }
    };

    // 2. Register session ownership
    {
        let mut owners = state.session_owners.lock().await;
        owners.insert(session_id.to_string(), conn_id.to_string());
    }

    // 3. Determine agent ID (default to primary)
    let agent_id = PRIMARY_AGENT_ID.to_string();

    // 4. Register in connection state
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.clone(), session_id.to_string());
        }
    }

    // 5. Register agent mapping
    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id);
    }

    // 6. Send loaded audit view
    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            audit,
        },
    )
    .await;

    // 7. Send updated state
    send_state(state, conn_id, tx).await;
}

async fn send_message(tx: &mpsc::Sender<String>, message: UiServerMessage) -> bool {
    match serde_json::to_string(&message) {
        Ok(json) => tx.send(json).await.is_ok(),
        Err(err) => {
            log::error!("Failed to serialize UI message: {}", err);
            false
        }
    }
}

async fn send_error(tx: &mpsc::Sender<String>, message: String) -> bool {
    send_message(tx, UiServerMessage::Error { message }).await
}

fn spawn_event_forwarders(state: ServerState, conn_id: String, tx: mpsc::Sender<String>) {
    for event_source in &state.event_sources {
        let mut events = event_source.subscribe();
        let tx_events = tx.clone();
        let conn_id_events = conn_id.clone();
        let state_events = state.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if !is_event_owned(&state_events, &conn_id_events, &event).await {
                    continue;
                }

                let agent_id = {
                    let agents = state_events.session_agents.lock().await;
                    agents
                        .get(&event.session_id)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string())
                };

                if !send_message(
                    &tx_events,
                    UiServerMessage::Event {
                        agent_id,
                        event: event.clone(),
                    },
                )
                .await
                {
                    break;
                }
            }
        });
    }
}

async fn is_event_owned(state: &ServerState, conn_id: &str, event: &AgentEvent) -> bool {
    if let AgentEventKind::SessionForked {
        parent_session_id,
        child_session_id,
        target_agent_id,
        origin,
        ..
    } = &event.kind
        && matches!(origin, ForkOrigin::Delegation)
    {
        let mut owners = state.session_owners.lock().await;
        if let Some(owner) = owners.get(parent_session_id).cloned() {
            owners.insert(child_session_id.clone(), owner);
            drop(owners);
            let mut agents = state.session_agents.lock().await;
            agents.insert(child_session_id.clone(), target_agent_id.clone());
        }
    }

    let owners = state.session_owners.lock().await;
    owners
        .get(&event.session_id)
        .map(|owner| owner == conn_id)
        .unwrap_or(false)
}
