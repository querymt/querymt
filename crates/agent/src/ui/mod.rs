use crate::agent::QueryMTAgent;
use crate::event_bus::EventBus;
use crate::events::{AgentEvent, AgentEventKind};
use crate::index::{
    FileIndex, FileIndexEntry, WorkspaceIndexManager, normalize_cwd, resolve_workspace_root,
};
use crate::send_agent::SendAgent;
use crate::session::domain::ForkOrigin;
use crate::session::projection::{AuditView, ViewStore};
use agent_client_protocol::{
    ContentBlock, ImageContent, NewSessionRequest, PromptRequest, TextContent,
};
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::get,
};
use base64::Engine;
use futures_util::{sink::SinkExt, stream::StreamExt as FuturesStreamExt};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use uuid::Uuid;

const PRIMARY_AGENT_ID: &str = "primary";
static FILE_MENTION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"@\{(file|dir):([^}]+)\}").unwrap());

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
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    workspace_manager: Arc<WorkspaceIndexManager>,
}

#[derive(Clone)]
struct ServerState {
    agent: Arc<QueryMTAgent>,
    view_store: Arc<dyn ViewStore>,
    event_sources: Vec<Arc<EventBus>>,
    connections: Arc<Mutex<HashMap<String, ConnectionState>>>,
    session_owners: Arc<Mutex<HashMap<String, String>>>,
    session_agents: Arc<Mutex<HashMap<String, String>>>,
    session_cwds: Arc<Mutex<HashMap<String, PathBuf>>>,
    workspace_manager: Arc<WorkspaceIndexManager>,
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
            agent: agent.clone(),
            view_store,
            event_sources,
            connections: Arc::new(Mutex::new(HashMap::new())),
            session_owners: Arc::new(Mutex::new(HashMap::new())),
            session_agents: Arc::new(Mutex::new(HashMap::new())),
            session_cwds: Arc::new(Mutex::new(HashMap::new())),
            workspace_manager: agent.workspace_index_manager(),
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
            session_cwds: self.session_cwds,
            workspace_manager: self.workspace_manager,
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
    cwd: Option<String>,
    title: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Serialize)]
struct SessionGroup {
    cwd: Option<String>,
    sessions: Vec<SessionSummary>,
    latest_activity: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum UiClientMessage {
    Init,
    SetActiveAgent {
        agent_id: String,
    },
    SetRoutingMode {
        mode: RoutingMode,
    },
    NewSession {
        cwd: Option<String>,
    },
    Prompt {
        text: String,
    },
    ListSessions,
    LoadSession {
        session_id: String,
    },
    /// Request file index for @ mentions
    GetFileIndex,
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
        groups: Vec<SessionGroup>,
    },
    SessionLoaded {
        session_id: String,
        audit: AuditView,
    },
    WorkspaceIndexStatus {
        session_id: String,
        status: String,
        message: Option<String>,
    },
    /// File index for autocomplete
    FileIndex {
        files: Vec<FileIndexEntry>,
        generated_at: u64,
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
                    let mut cwds = state.session_cwds.lock().await;
                    for sid in session_ids {
                        owners.remove(&sid);
                        agents.remove(&sid);
                        cwds.remove(&sid);
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
        UiClientMessage::GetFileIndex => {
            log::debug!("Received GetFileIndex message from conn_id={}", conn_id);
            handle_get_file_index(state, conn_id, tx).await;
            log::debug!("GetFileIndex handler completed for conn_id={}", conn_id);
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
            let session_cwd = session_cwd_for(state, &session_id).await.or(cwd.cloned());
            let prompt_blocks = build_prompt_blocks(state, session_cwd.as_ref(), text).await;
            send_prompt(agent, session_id, prompt_blocks).await?;
        }
        RoutingMode::Broadcast => {
            let agent_ids = list_agent_ids(state);
            for agent_id in agent_ids {
                let session_id = ensure_session(state, conn_id, &agent_id, cwd, tx).await?;
                let agent = agent_for_id(state, &agent_id)
                    .ok_or_else(|| format!("Unknown agent: {}", agent_id))?;
                let session_cwd = session_cwd_for(state, &session_id).await.or(cwd.cloned());
                let prompt_blocks = build_prompt_blocks(state, session_cwd.as_ref(), text).await;
                send_prompt(agent, session_id, prompt_blocks).await?;
            }
        }
    }
    Ok(())
}

async fn send_prompt(
    agent: Arc<dyn SendAgent>,
    session_id: String,
    prompt: Vec<ContentBlock>,
) -> Result<(), String> {
    let request = PromptRequest::new(session_id, prompt);
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

    if let Some(cwd_path) = cwd.cloned() {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.clone(), cwd_path);
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

    if let Some(cwd_path) = cwd.cloned() {
        let root = resolve_workspace_root(&cwd_path);
        let manager = state.workspace_manager.clone();
        let session_id_clone = session_id.clone();
        let tx_clone = tx.clone();

        let _ = send_message(
            tx,
            UiServerMessage::WorkspaceIndexStatus {
                session_id: session_id.clone(),
                status: "building".to_string(),
                message: None,
            },
        )
        .await;

        tokio::spawn(async move {
            let status = match manager.get_or_create(root).await {
                Ok(_) => UiServerMessage::WorkspaceIndexStatus {
                    session_id: session_id_clone,
                    status: "ready".to_string(),
                    message: None,
                },
                Err(err) => UiServerMessage::WorkspaceIndexStatus {
                    session_id: session_id_clone,
                    status: "error".to_string(),
                    message: Some(err.to_string()),
                },
            };

            let _ = send_message(&tx_clone, status).await;
        });
    }

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
    cwd.map(|path| normalize_cwd(&PathBuf::from(path)))
}

async fn session_cwd_for(state: &ServerState, session_id: &str) -> Option<PathBuf> {
    let cwds = state.session_cwds.lock().await;
    cwds.get(session_id).cloned()
}

async fn build_prompt_blocks(
    state: &ServerState,
    cwd: Option<&PathBuf>,
    text: &str,
) -> Vec<ContentBlock> {
    let Some(cwd) = cwd else {
        return vec![ContentBlock::Text(TextContent::new(text.to_string()))];
    };

    let (expanded, mut attachments) = expand_prompt_mentions(state, cwd, text).await;
    let mut blocks = Vec::with_capacity(1 + attachments.len());
    blocks.push(ContentBlock::Text(TextContent::new(expanded)));
    blocks.append(&mut attachments);
    blocks
}

async fn expand_prompt_mentions(
    state: &ServerState,
    cwd: &Path,
    text: &str,
) -> (String, Vec<ContentBlock>) {
    if !text.contains("@{") {
        return (text.to_string(), Vec::new());
    }

    let root = resolve_workspace_root(cwd);
    let index_lookup = build_file_index_lookup(state, cwd, &root).await;
    let mut output = String::new();
    let mut attachments = Vec::new();
    let mut blocks = Vec::new();
    let mut seen = HashSet::new();
    let mut last_index = 0;

    for captures in FILE_MENTION_RE.captures_iter(text) {
        let full_match = captures.get(0).unwrap();
        output.push_str(&text[last_index..full_match.start()]);
        last_index = full_match.end();

        let kind = captures.get(1).map(|m| m.as_str()).unwrap_or("file");
        let raw_path = captures.get(2).map(|m| m.as_str()).unwrap_or("").trim();
        if raw_path.is_empty() {
            output.push_str(full_match.as_str());
            continue;
        }

        let expected_is_dir = kind == "dir";
        if let Some(index_lookup) = &index_lookup {
            match index_lookup.get(raw_path) {
                Some(is_dir) if *is_dir == expected_is_dir => {}
                _ => {
                    output.push_str(full_match.as_str());
                    continue;
                }
            }
        }

        let resolved_path = cwd.join(raw_path);
        let resolved_path = match resolved_path.canonicalize() {
            Ok(path) => path,
            Err(_) => {
                output.push_str(full_match.as_str());
                continue;
            }
        };
        if !resolved_path.starts_with(&root) {
            output.push_str(full_match.as_str());
            continue;
        }

        output.push_str(&format!("[{}: {}]", kind, raw_path));

        let seen_key = format!("{}:{}", kind, raw_path);
        if !seen.insert(seen_key) {
            continue;
        }

        if expected_is_dir {
            attachments.push(format_dir_attachment(raw_path, &resolved_path));
            continue;
        }

        let bytes = match std::fs::read(&resolved_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                attachments.push(format!("[file: {}]\n(file could not be read)", raw_path));
                continue;
            }
        };

        if let Some(mime_type) = detect_image_mime(&bytes) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let image = ImageContent::new(encoded, mime_type).uri(raw_path.to_string());
            blocks.push(ContentBlock::Image(image));
            attachments.push(format!("[file: {}]\n(image attached)", raw_path));
            continue;
        }

        match String::from_utf8(bytes) {
            Ok(content) => attachments.push(format!("[file: {}]\n```\n{}\n```", raw_path, content)),
            Err(_) => attachments.push(format!("[file: {}]\n(binary file; not inlined)", raw_path)),
        }
    }

    output.push_str(&text[last_index..]);

    if !attachments.is_empty() {
        output.push_str("\n\nAttachments:\n");
        output.push_str(&attachments.join("\n\n"));
    }

    (output, blocks)
}

async fn build_file_index_lookup(
    state: &ServerState,
    cwd: &Path,
    root: &Path,
) -> Option<HashMap<String, bool>> {
    let workspace = state
        .workspace_manager
        .get_or_create(root.to_path_buf())
        .await
        .ok()?;
    let index = workspace.file_index()?;
    let relative_cwd = cwd.strip_prefix(root).ok()?;
    let entries = filter_index_for_cwd(&index, relative_cwd);
    let mut lookup = HashMap::new();
    for entry in entries {
        lookup.insert(entry.path, entry.is_dir);
    }
    Some(lookup)
}

fn format_dir_attachment(display_path: &str, resolved_path: &Path) -> String {
    let mut entries = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(resolved_path) {
        for entry in read_dir.flatten() {
            let file_type = entry.file_type().ok();
            let mut name = entry.file_name().to_string_lossy().to_string();
            if file_type.map(|ft| ft.is_dir()).unwrap_or(false) {
                name.push('/');
            }
            entries.push(name);
        }
    }
    entries.sort();

    if entries.is_empty() {
        return format!("[dir: {}]\n(empty directory)", display_path);
    }

    let listing = entries
        .into_iter()
        .map(|entry| format!("- {}", entry))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[dir: {}]\n{}", display_path, listing)
}

fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    let kind = infer::get(bytes)?;
    match kind.mime_type() {
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Some(kind.mime_type()),
        _ => None,
    }
}

async fn handle_list_sessions(state: &ServerState, tx: &mpsc::Sender<String>) {
    // Use ViewStore to get pre-grouped session list
    let view = match state.view_store.get_session_list_view(None).await {
        Ok(view) => view,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list sessions: {}", e)).await;
            return;
        }
    };

    // Convert to UI message format
    let groups: Vec<SessionGroup> = view
        .groups
        .into_iter()
        .map(|g| SessionGroup {
            cwd: g.cwd,
            latest_activity: g.latest_activity.and_then(|t| t.format(&Rfc3339).ok()),
            sessions: g
                .sessions
                .into_iter()
                .map(|s| SessionSummary {
                    session_id: s.session_id,
                    name: s.name,
                    cwd: s.cwd,
                    title: s.title,
                    created_at: s.created_at.and_then(|t| t.format(&Rfc3339).ok()),
                    updated_at: s.updated_at.and_then(|t| t.format(&Rfc3339).ok()),
                })
                .collect(),
        })
        .collect();

    let _ = send_message(tx, UiServerMessage::SessionList { groups }).await;
}

async fn handle_get_file_index(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    log::debug!("handle_get_file_index: called for conn_id={}", conn_id);

    let session_id = {
        let connections = state.connections.lock().await;
        connections.get(conn_id).and_then(|conn| {
            log::debug!(
                "handle_get_file_index: active_agent_id={}, sessions={:?}",
                conn.active_agent_id,
                conn.sessions.keys().collect::<Vec<_>>()
            );
            conn.sessions.get(&conn.active_agent_id).cloned()
        })
    };
    log::debug!("handle_get_file_index: session_id={:?}", session_id);

    let Some(session_id) = session_id else {
        log::warn!("handle_get_file_index: No active session, sending error");
        let _ = send_error(tx, "No active session".to_string()).await;
        return;
    };

    let cwd = {
        let cwds = state.session_cwds.lock().await;
        cwds.get(&session_id).cloned()
    };
    log::debug!("handle_get_file_index: cwd={:?}", cwd);

    let Some(cwd) = cwd else {
        log::warn!(
            "handle_get_file_index: No working directory set for session {}",
            session_id
        );
        let _ = send_error(tx, "No working directory set for this session".to_string()).await;
        return;
    };

    let root = resolve_workspace_root(&cwd);
    log::debug!("handle_get_file_index: root={:?}", root);

    log::debug!("handle_get_file_index: calling get_or_create for workspace");
    let workspace = match state.workspace_manager.get_or_create(root.clone()).await {
        Ok(workspace) => {
            log::debug!("handle_get_file_index: got workspace");
            workspace
        }
        Err(err) => {
            log::error!("handle_get_file_index: workspace error: {}", err);
            let _ = send_error(tx, format!("Workspace index error: {}", err)).await;
            return;
        }
    };

    log::debug!("handle_get_file_index: calling workspace.file_index()");
    let Some(index) = workspace.file_index() else {
        log::warn!("handle_get_file_index: File index not ready");
        let _ = send_error(tx, "File index not ready".to_string()).await;
        return;
    };
    log::debug!(
        "handle_get_file_index: got index with {} files",
        index.files.len()
    );

    let relative_cwd = match cwd.strip_prefix(&root) {
        Ok(relative) => relative,
        Err(_) => {
            log::warn!(
                "handle_get_file_index: cwd {:?} is outside workspace root {:?}",
                cwd,
                root
            );
            let _ = send_error(tx, "Working directory outside workspace root".to_string()).await;
            return;
        }
    };
    log::debug!("handle_get_file_index: relative_cwd={:?}", relative_cwd);

    let files = filter_index_for_cwd(&index, relative_cwd);
    log::debug!(
        "handle_get_file_index: filtered to {} files, sending response",
        files.len()
    );

    let send_result = send_message(
        tx,
        UiServerMessage::FileIndex {
            files,
            generated_at: index.generated_at,
        },
    )
    .await;

    if let Err(e) = send_result {
        log::error!("handle_get_file_index: failed to send response: {}", e);
    } else {
        log::debug!("handle_get_file_index: response sent successfully");
    }
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

    // 1a. Load session to get cwd and populate session_cwds
    if let Ok(Some(session)) = state.agent.provider.get_session(session_id).await
        && let Some(cwd) = session.cwd
    {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.to_string(), cwd);
    }

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

async fn send_message(tx: &mpsc::Sender<String>, message: UiServerMessage) -> Result<(), String> {
    let message_type = match &message {
        UiServerMessage::State { .. } => "state",
        UiServerMessage::SessionCreated { .. } => "session_created",
        UiServerMessage::Event { .. } => "event",
        UiServerMessage::Error { .. } => "error",
        UiServerMessage::SessionList { .. } => "session_list",
        UiServerMessage::SessionLoaded { .. } => "session_loaded",
        UiServerMessage::WorkspaceIndexStatus { .. } => "workspace_index_status",
        UiServerMessage::FileIndex { .. } => "file_index",
    };

    match serde_json::to_string(&message) {
        Ok(json) => {
            log::debug!(
                "send_message: sending {} (length: {})",
                message_type,
                json.len()
            );
            match tx.send(json).await {
                Ok(_) => {
                    log::debug!("send_message: {} sent successfully", message_type);
                    Ok(())
                }
                Err(e) => {
                    log::error!("send_message: failed to send {}: {}", message_type, e);
                    Err(format!("Failed to send: {}", e))
                }
            }
        }
        Err(err) => {
            log::error!(
                "send_message: failed to serialize {}: {}",
                message_type,
                err
            );
            Err(format!("Failed to serialize: {}", err))
        }
    }
}

async fn send_error(tx: &mpsc::Sender<String>, message: String) -> Result<(), String> {
    log::debug!("send_error: sending error message: {}", message);
    send_message(tx, UiServerMessage::Error { message }).await
}

fn filter_index_for_cwd(index: &FileIndex, relative_cwd: &Path) -> Vec<FileIndexEntry> {
    if relative_cwd.as_os_str().is_empty() {
        return index.files.clone();
    }

    index
        .files
        .iter()
        .filter_map(|entry| {
            let entry_path = Path::new(&entry.path);
            if !entry_path.starts_with(relative_cwd) {
                return None;
            }

            let relative_path = entry_path.strip_prefix(relative_cwd).ok()?;
            if relative_path.as_os_str().is_empty() {
                return None;
            }

            Some(FileIndexEntry {
                path: relative_path.to_string_lossy().to_string(),
                is_dir: entry.is_dir,
            })
        })
        .collect()
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

                if send_message(
                    &tx_events,
                    UiServerMessage::Event {
                        agent_id,
                        event: event.clone(),
                    },
                )
                .await
                .is_err()
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
