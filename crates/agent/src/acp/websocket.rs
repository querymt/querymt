//! Standalone WebSocket ACP server.
//!
//! This module provides a pure ACP server over WebSocket without any dashboard UI.
//! It uses the same client bridge pattern as the stdio transport for consistency.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use querymt_agent::api::Agent;
//! use querymt_agent::acp::websocket::serve_websocket;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let agent = Agent::single()
//!     .provider("anthropic", "claude-sonnet-4-20250514")
//!     .build()
//!     .await?;
//!
//! // Start standalone WebSocket server on ws://127.0.0.1:3030/ws
//! serve_websocket(agent.inner(), "127.0.0.1:3030").await?;
//! # Ok(())
//! # }
//! ```

use crate::acp::protocol::CreateElicitationRequest;
use crate::acp::shared::{
    AcpLiveEventTranslator, PendingElicitationMap, PermissionMap, RpcMessage, SessionOwnerMap,
    collect_event_sources, convert_elicitation_response_value, create_elicitation_request,
    dispatch_rpc_message, is_event_owned,
};
use crate::acp::shutdown;
use crate::event_fanout::EventFanout;
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
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// State for standalone WebSocket ACP server
#[derive(Clone)]
pub(crate) struct WsServerState {
    pub(crate) agent: Arc<crate::agent::LocalAgentHandle>,
    pub(crate) pending_permissions: PermissionMap,
    pub(crate) pending_elicitations: PendingElicitationMap,
    pub(crate) event_sources: Vec<Arc<EventFanout>>,
    pub(crate) session_owners: SessionOwnerMap,
}

impl WsServerState {
    pub(crate) fn new(agent: Arc<crate::agent::LocalAgentHandle>) -> Self {
        Self {
            event_sources: collect_event_sources(&agent),
            pending_permissions: Arc::new(Mutex::new(HashMap::new())),
            pending_elicitations: agent.pending_elicitations(),
            session_owners: Arc::new(Mutex::new(HashMap::new())),
            agent,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PendingWsElicitation {
    pub(crate) session_id: String,
    pub(crate) elicitation_id: String,
    pub(crate) response_tx: oneshot::Sender<Result<serde_json::Value, serde_json::Value>>,
}

pub(crate) type PendingWsRequestMap = Arc<Mutex<HashMap<String, PendingWsElicitation>>>;

#[derive(Deserialize)]
#[serde(untagged)]
enum InboundWsMessage {
    Request(RpcMessage),
    Response {
        id: serde_json::Value,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        error: Option<serde_json::Value>,
    },
}

fn websocket_elicitation_request(
    request_id: &str,
    request: &CreateElicitationRequest,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "elicitation/create",
        "params": request,
    })
}

fn response_id_key(id: &serde_json::Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_string())
}

pub(crate) async fn route_websocket_response(
    pending_requests: &PendingWsRequestMap,
    id: serde_json::Value,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
) -> bool {
    let key = response_id_key(&id);
    let pending = pending_requests.lock().await.remove(&key);
    let Some(pending) = pending else {
        log::warn!(
            "Ignoring WebSocket response with unknown request id: {}",
            key
        );
        return false;
    };

    let response = match error {
        Some(error) => Err(error),
        None => Ok(result.unwrap_or(serde_json::Value::Null)),
    };
    if pending.response_tx.send(response).is_err() {
        log::debug!(
            "WebSocket elicitation response receiver dropped: session_id={} elicitation_id={}",
            pending.session_id,
            pending.elicitation_id
        );
    }
    true
}

pub(crate) async fn cancel_pending_websocket_requests(pending_requests: &PendingWsRequestMap) {
    let disconnected = {
        let mut pending = pending_requests.lock().await;
        pending
            .drain()
            .map(|(_, request)| request)
            .collect::<Vec<_>>()
    };
    for request in disconnected {
        let _ = request.response_tx.send(Err(serde_json::json!({
            "code": -32000,
            "message": "WebSocket connection closed",
        })));
    }
}

async fn resolve_websocket_elicitation(
    agent: &crate::agent::LocalAgentHandle,
    session_id: String,
    elicitation_id: String,
    response: crate::elicitation::ElicitationResponse,
) {
    match crate::elicitation::take_pending_elicitation_sender_for_session(
        agent,
        &session_id,
        &elicitation_id,
    )
    .await
    {
        Some(sender) => {
            if sender.send(response).is_err() {
                log::warn!(
                    "WebSocket elicitation receiver dropped: session_id={} elicitation_id={}",
                    session_id,
                    elicitation_id
                );
            } else {
                log::debug!(
                    "WebSocket elicitation response delivered: session_id={} elicitation_id={}",
                    session_id,
                    elicitation_id
                );
            }
        }
        None => log::warn!(
            "No pending WebSocket elicitation found: session_id={} elicitation_id={}",
            session_id,
            elicitation_id
        ),
    }
}

/// Run a standalone WebSocket ACP server.
///
/// This starts a WebSocket server that implements the Agent Client Protocol.
/// The server is standalone (no dashboard UI) and can be used as a pure ACP endpoint.
///
/// # Arguments
///
/// * `agent` - The QueryMTAgent instance to serve
/// * `addr` - The address to bind to (e.g., "127.0.0.1:3030")
///
/// # Example
///
/// ```rust,no_run
/// use querymt_agent::api::Agent;
/// use querymt_agent::acp::websocket::serve_websocket;
///
/// # async fn example() -> anyhow::Result<()> {
/// let agent = Agent::single()
///     .provider("anthropic", "claude-sonnet-4-20250514")
///     .cwd("/tmp")
///     .tools(["read_tool", "write_file", "shell"])
///     .build()
///     .await?;
///
/// println!("Starting WebSocket ACP server on ws://127.0.0.1:3030/ws");
/// serve_websocket(agent.inner(), "127.0.0.1:3030").await?;
/// # Ok(())
/// # }
/// ```
///
/// # Graceful Shutdown
///
/// The server handles SIGTERM and SIGINT (Ctrl+C) for graceful shutdown.
/// Active connections are gracefully closed before exit.
pub async fn serve_websocket(
    agent: Arc<crate::agent::LocalAgentHandle>,
    addr: &str,
) -> anyhow::Result<()> {
    log::info!("Starting standalone WebSocket ACP server on {}", addr);

    let app = router(agent);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    log::info!("WebSocket ACP server listening on ws://{}/ws", addr);
    log::info!("Press Ctrl+C to stop");

    // Run with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown::signal())
        .await?;

    log::info!("WebSocket ACP server shutdown complete");
    Ok(())
}

/// WebSocket upgrade handler
pub(crate) fn router(agent: Arc<crate::agent::LocalAgentHandle>) -> Router {
    Router::new()
        .route("/ws", get(websocket_handler))
        .with_state(WsServerState::new(agent))
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<WsServerState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_websocket_connection(socket, state))
}

/// Handle a WebSocket connection lifecycle
async fn handle_websocket_connection(socket: WebSocket, state: WsServerState) {
    let conn_id = Uuid::new_v4().to_string();
    log::info!("New WebSocket connection: {}", conn_id);

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<String>(100);
    let pending_requests: PendingWsRequestMap = Arc::new(Mutex::new(HashMap::new()));
    let forwarded_elicitations = Arc::new(Mutex::new(HashSet::new()));
    let request_counter = Arc::new(AtomicU64::new(1));
    let connection_cancel = CancellationToken::new();

    spawn_event_forwarders(
        state.clone(),
        ConnectionEventState {
            conn_id: conn_id.clone(),
            tx: tx.clone(),
            pending_requests: pending_requests.clone(),
            forwarded_elicitations,
            request_counter,
            connection_cancel: connection_cancel.clone(),
        },
    );

    let mut send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    let conn_id_receive = conn_id.clone();
    let state_receive = state.clone();
    let tx_receive = tx.clone();
    let pending_receive = pending_requests.clone();
    let mut receive_task = tokio::spawn(async move {
        while let Some(result) = FuturesStreamExt::next(&mut ws_receiver).await {
            match result {
                Ok(Message::Text(text)) => match serde_json::from_str::<InboundWsMessage>(&text) {
                    Ok(InboundWsMessage::Request(request)) => {
                        tokio::spawn(dispatch_rpc_message(
                            state_receive.agent.clone(),
                            state_receive.session_owners.clone(),
                            state_receive.pending_permissions.clone(),
                            state_receive.pending_elicitations.clone(),
                            conn_id_receive.clone(),
                            request,
                            tx_receive.clone(),
                        ));
                    }
                    Ok(InboundWsMessage::Response { id, result, error }) => {
                        route_websocket_response(&pending_receive, id, result, error).await;
                    }
                    Err(err) => {
                        log::error!("Failed to parse WebSocket JSON-RPC message: {}", err);
                    }
                },
                Ok(Message::Close(_)) => {
                    log::info!("WebSocket closed by client: {}", conn_id_receive);
                    break;
                }
                Ok(Message::Ping(_)) => {
                    log::trace!("Received ping from {}", conn_id_receive);
                }
                Ok(_) => {}
                Err(err) => {
                    log::error!("WebSocket error for {}: {}", conn_id_receive, err);
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => {},
        _ = &mut receive_task => {},
    }
    connection_cancel.cancel();
    send_task.abort();
    receive_task.abort();

    cancel_pending_websocket_requests(&pending_requests).await;

    let mut owners = state.session_owners.lock().await;
    owners.retain(|_, owner| owner != &conn_id);
    log::info!("WebSocket connection closed: {}", conn_id);
}

/// Spawn event forwarders that subscribe to event buses and forward events to the client.
///
/// For each event source (agent event bus), this spawns a task that:
/// 1. Subscribes to the event bus
/// 2. Filters events owned by this connection
/// 3. Translates events to JSON-RPC notifications
/// 4. Sends notifications to the client via the mpsc channel
#[derive(Clone)]
pub(crate) struct ConnectionEventState {
    pub(crate) conn_id: String,
    pub(crate) tx: mpsc::Sender<String>,
    pub(crate) pending_requests: PendingWsRequestMap,
    pub(crate) forwarded_elicitations: Arc<Mutex<HashSet<(String, String)>>>,
    pub(crate) request_counter: Arc<AtomicU64>,
    pub(crate) connection_cancel: CancellationToken,
}

pub(crate) fn spawn_event_forwarders(state: WsServerState, connection: ConnectionEventState) {
    for event_source in &state.event_sources {
        let mut events = event_source.subscribe();
        let tx_events = connection.tx.clone();
        let conn_id_events = connection.conn_id.clone();
        let state_events = state.clone();
        let pending_events = connection.pending_requests.clone();
        let forwarded_events = connection.forwarded_elicitations.clone();
        let request_counter = connection.request_counter.clone();
        let connection_cancel = connection.connection_cancel.clone();

        tokio::spawn(async move {
            let mut translator = AcpLiveEventTranslator::new();
            loop {
                let event = tokio::select! {
                    _ = connection_cancel.cancelled() => break,
                    result = events.recv() => match result {
                        Ok(event) => event,
                        Err(_) => break,
                    },
                };
                if !is_event_owned(&state_events.session_owners, &conn_id_events, &event).await {
                    continue;
                }

                if let crate::events::AgentEventKind::ElicitationRequested {
                    elicitation_id,
                    session_id,
                    message,
                    requested_schema,
                    source,
                } = event.kind()
                {
                    let key = (session_id.clone(), elicitation_id.clone());
                    if !forwarded_events.lock().await.insert(key) {
                        continue;
                    }

                    let request = match create_elicitation_request(
                        elicitation_id.clone(),
                        session_id.clone(),
                        message.clone(),
                        requested_schema.clone(),
                        source.clone(),
                    ) {
                        Ok(request) => request,
                        Err(err) => {
                            log::warn!(
                                "Invalid WebSocket elicitation request: session_id={} elicitation_id={} error={}",
                                session_id,
                                elicitation_id,
                                err
                            );
                            resolve_websocket_elicitation(
                                &state_events.agent,
                                session_id.clone(),
                                elicitation_id.clone(),
                                crate::elicitation::ElicitationResponse {
                                    action: crate::elicitation::ElicitationAction::Cancel,
                                    content: None,
                                },
                            )
                            .await;
                            continue;
                        }
                    };

                    let request_id = format!(
                        "querymt:{}:{}",
                        conn_id_events,
                        request_counter.fetch_add(1, Ordering::Relaxed)
                    );
                    let request_key =
                        response_id_key(&serde_json::Value::String(request_id.clone()));
                    let (response_tx, response_rx) = oneshot::channel();
                    pending_events.lock().await.insert(
                        request_key.clone(),
                        PendingWsElicitation {
                            session_id: session_id.clone(),
                            elicitation_id: elicitation_id.clone(),
                            response_tx,
                        },
                    );

                    let wire_request = websocket_elicitation_request(&request_id, &request);
                    let json = match serde_json::to_string(&wire_request) {
                        Ok(json) => json,
                        Err(err) => {
                            pending_events.lock().await.remove(&request_key);
                            log::warn!("Failed to serialize WebSocket elicitation: {}", err);
                            resolve_websocket_elicitation(
                                &state_events.agent,
                                session_id.clone(),
                                elicitation_id.clone(),
                                crate::elicitation::ElicitationResponse {
                                    action: crate::elicitation::ElicitationAction::Cancel,
                                    content: None,
                                },
                            )
                            .await;
                            continue;
                        }
                    };
                    if tx_events.send(json).await.is_err() {
                        pending_events.lock().await.remove(&request_key);
                        resolve_websocket_elicitation(
                            &state_events.agent,
                            session_id.clone(),
                            elicitation_id.clone(),
                            crate::elicitation::ElicitationResponse {
                                action: crate::elicitation::ElicitationAction::Cancel,
                                content: None,
                            },
                        )
                        .await;
                        break;
                    }

                    let agent = state_events.agent.clone();
                    let session_id = session_id.clone();
                    let elicitation_id = elicitation_id.clone();
                    tokio::spawn(async move {
                        let response = match response_rx.await {
                            Ok(Ok(value)) => match convert_elicitation_response_value(value) {
                                Ok(response) => response,
                                Err(err) => {
                                    log::warn!(
                                        "Invalid WebSocket elicitation response: session_id={} elicitation_id={} error={}",
                                        session_id,
                                        elicitation_id,
                                        err
                                    );
                                    crate::elicitation::ElicitationResponse {
                                        action: crate::elicitation::ElicitationAction::Cancel,
                                        content: None,
                                    }
                                }
                            },
                            Ok(Err(error)) => {
                                log::warn!(
                                    "WebSocket elicitation request failed: session_id={} elicitation_id={} error={}",
                                    session_id,
                                    elicitation_id,
                                    error
                                );
                                crate::elicitation::ElicitationResponse {
                                    action: crate::elicitation::ElicitationAction::Cancel,
                                    content: None,
                                }
                            }
                            Err(_) => crate::elicitation::ElicitationResponse {
                                action: crate::elicitation::ElicitationAction::Cancel,
                                content: None,
                            },
                        };
                        resolve_websocket_elicitation(&agent, session_id, elicitation_id, response)
                            .await;
                    });
                    continue;
                }

                if let Some(notification) = translator.translate_notification(&event) {
                    let json = serde_json::to_string(&notification).unwrap_or_default();
                    if tx_events.send(json).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}
