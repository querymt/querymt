// Core ACP infrastructure
pub mod shutdown;
pub mod transport;
pub mod websocket;

// Internal modules
pub(crate) mod client_bridge;
pub(crate) mod shared;
pub(crate) mod stdio;

// Public re-exports
pub use transport::AcpTransport;
pub use websocket::serve_websocket;

// Re-export for backward compatibility with existing examples
pub use stdio::run_sdk_stdio;
pub use stdio::serve_stdio;

// Existing manual JSON-RPC implementation (for dashboard compatibility)
use crate::acp::shared::{
    PermissionMap, RpcRequest, SessionOwnerMap, collect_event_sources, handle_rpc_message,
    is_event_owned, translate_event_to_notification,
};
use crate::agent::QueryMTAgent;
use crate::event_bus::EventBus;
use agent_client_protocol::{
    Error, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
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
use futures_util::{sink::SinkExt, stream::StreamExt as FuturesStreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};
use uuid::Uuid;

pub struct AcpServer {
    agent: Arc<QueryMTAgent>,
    pending_permissions: PermissionMap,
    event_sources: Vec<Arc<EventBus>>,
    session_owners: SessionOwnerMap,
}

#[derive(Clone)]
struct ServerState {
    agent: Arc<QueryMTAgent>,
    pending_permissions: PermissionMap,
    event_sources: Vec<Arc<EventBus>>,
    session_owners: SessionOwnerMap,
}

impl AcpServer {
    pub fn new(agent: Arc<QueryMTAgent>) -> Self {
        let event_sources = collect_event_sources(&agent);
        let pending_permissions = Arc::new(Mutex::new(HashMap::new()));
        let session_owners = Arc::new(Mutex::new(HashMap::new()));

        let client = WebClientBridge {
            pending_permissions: pending_permissions.clone(),
        };
        agent.set_client(Arc::new(client));

        Self {
            agent,
            pending_permissions,
            event_sources,
            session_owners,
        }
    }

    pub fn router(self) -> Router {
        let state = ServerState {
            agent: self.agent,
            pending_permissions: self.pending_permissions,
            event_sources: self.event_sources,
            session_owners: self.session_owners,
        };

        Router::new()
            .route("/ws", get(websocket_handler))
            .with_state(state)
    }

    pub async fn run_stdio(self) -> anyhow::Result<()> {
        let state = ServerState {
            agent: self.agent,
            pending_permissions: self.pending_permissions,
            event_sources: self.event_sources,
            session_owners: self.session_owners,
        };

        let conn_id = "stdio".to_string();
        let (tx, mut rx) = mpsc::channel::<String>(100);

        spawn_event_forwarders(state.clone(), conn_id.clone(), tx.clone());

        let send_task = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(msg) = rx.recv().await {
                if stdout.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                if stdout.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdout.flush().await;
            }
        });

        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<RpcRequest>(&line) {
                Ok(request) => {
                    let response = handle_rpc_message(
                        state.agent.as_ref(),
                        &state.session_owners,
                        &state.pending_permissions,
                        &conn_id,
                        request,
                    )
                    .await;
                    let json = serde_json::to_string(&response).unwrap_or_default();
                    if tx.send(json).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::error!("Failed to parse stdio message: {}", e);
                }
            }
        }

        send_task.abort();

        Ok(())
    }
}

struct WebClientBridge {
    pending_permissions: Arc<Mutex<HashMap<String, oneshot::Sender<RequestPermissionOutcome>>>>,
}

#[async_trait::async_trait(?Send)]
impl agent_client_protocol::Client for WebClientBridge {
    async fn session_notification(
        &self,
        _notif: agent_client_protocol::SessionNotification,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        let (tx, rx) = oneshot::channel();
        let tool_call_id = req.tool_call.tool_call_id.0.clone();

        {
            let mut pending = self.pending_permissions.lock().await;
            pending.insert(tool_call_id.to_string(), tx);
        }

        match rx.await {
            Ok(outcome) => Ok(RequestPermissionResponse::new(outcome)),
            Err(_) => Err(Error::new(-32000, "Permission request cancelled")),
        }
    }
}

// Event translation functions and RPC types are now in shared.rs

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

    spawn_event_forwarders(state.clone(), conn_id.clone(), tx.clone());

    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    let receive_task = tokio::spawn(async move {
        while let Some(result) = FuturesStreamExt::next(&mut ws_receiver).await {
            match result {
                Ok(Message::Text(text)) => match serde_json::from_str::<RpcRequest>(&text) {
                    Ok(request) => {
                        let response = handle_rpc_message(
                            state.agent.as_ref(),
                            &state.session_owners,
                            &state.pending_permissions,
                            &conn_id,
                            request,
                        )
                        .await;
                        let json = serde_json::to_string(&response).unwrap_or_default();

                        if tx.send(json).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to parse WebSocket message: {}", e);
                    }
                },
                Ok(Message::Close(_)) => {
                    log::info!("WebSocket closed by client");
                    break;
                }
                Ok(Message::Ping(_)) => {
                    log::trace!("Received ping");
                }
                Ok(_) => {}
                Err(e) => {
                    log::error!("WebSocket error: {}", e);
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = receive_task => {},
    }
}

fn spawn_event_forwarders(state: ServerState, conn_id: String, tx: mpsc::Sender<String>) {
    for event_source in &state.event_sources {
        let mut events = event_source.subscribe();
        let tx_events = tx.clone();
        let conn_id_events = conn_id.clone();
        let session_owners = state.session_owners.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if !is_event_owned(&session_owners, &conn_id_events, &event).await {
                    continue;
                }
                if let Some(notification) = translate_event_to_notification(&event) {
                    let json = serde_json::to_string(&notification).unwrap_or_default();
                    if tx_events.send(json).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}

// The old handle_rpc_message function has been removed - now using shared::handle_rpc_message
