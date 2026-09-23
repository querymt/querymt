//! Standalone WebSocket ACP server.
//!
//! This module provides a pure ACP server over WebSocket without any dashboard UI.
//! Session catalog updates are advertised on the JSON-RPC dispatch path after
//! `session/new`, `session/load`, and `session/resume`. Live events fan out per
//! subscribed connection, while interactive ACP requests use a session-scoped bridge.
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
//! // Start standalone WebSocket server on ws://127.0.0.1:3030/acp/ws
//! serve_websocket(agent.inner(), "127.0.0.1:3030").await?;
//! # Ok(())
//! # }
//! ```

use crate::acp::client_bridge::{ClientBridgeMessage, ClientBridgeSender};
use crate::acp::protocol::{Error, ExtNotification, RequestPermissionResponse};
use crate::acp::shared::{
    AcpLiveEventTranslator, PendingElicitationMap, PermissionMap, RpcDispatchContext,
    RpcDispatchState, RpcMessage, SessionOwnerMap, collect_event_sources,
    convert_elicitation_response_value, create_elicitation_request,
    dispatch_rpc_message_with_context, is_event_owned,
};
use crate::acp::shutdown;
use crate::event_fanout::EventFanout;
use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::{sink::SinkExt, stream::StreamExt as FuturesStreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::Duration;
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
    pub(crate) connection_bridges: Arc<Mutex<HashMap<String, ClientBridgeSender>>>,
    session_reconciliation_locks: Arc<Mutex<HashMap<String, Weak<Mutex<()>>>>>,
}

impl WsServerState {
    pub(crate) fn new(agent: Arc<crate::agent::LocalAgentHandle>) -> Self {
        Self {
            event_sources: collect_event_sources(&agent),
            pending_permissions: Arc::new(Mutex::new(HashMap::new())),
            pending_elicitations: agent.pending_elicitations(),
            session_owners: SessionOwnerMap::default(),
            connection_bridges: Arc::new(Mutex::new(HashMap::new())),
            session_reconciliation_locks: Arc::new(Mutex::new(HashMap::new())),
            agent,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PendingWsRequest {
    pub(crate) method: &'static str,
    pub(crate) response_tx: oneshot::Sender<Result<serde_json::Value, serde_json::Value>>,
}

pub(crate) type PendingWsRequestMap = Arc<Mutex<HashMap<String, PendingWsRequest>>>;

const WEBSOCKET_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

struct PendingWsResponse {
    request_key: String,
    response_rx: oneshot::Receiver<Result<serde_json::Value, serde_json::Value>>,
}

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

fn websocket_request<T: serde::Serialize>(
    request_id: &str,
    method: &'static str,
    request: &T,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": method,
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
            "WebSocket response receiver dropped: method={} request_id={}",
            pending.method,
            key
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

async fn send_websocket_request<T: serde::Serialize>(
    tx: &mpsc::Sender<String>,
    pending_requests: &PendingWsRequestMap,
    request_counter: &AtomicU64,
    conn_id: &str,
    method: &'static str,
    params: &T,
) -> Result<PendingWsResponse, Error> {
    let request_id = format!(
        "querymt:{}:{}",
        conn_id,
        request_counter.fetch_add(1, Ordering::Relaxed)
    );
    let request_key = response_id_key(&serde_json::Value::String(request_id.clone()));
    let (response_tx, response_rx) = oneshot::channel();
    pending_requests.lock().await.insert(
        request_key.clone(),
        PendingWsRequest {
            method,
            response_tx,
        },
    );

    let json = match serde_json::to_string(&websocket_request(&request_id, method, params)) {
        Ok(json) => json,
        Err(err) => {
            pending_requests.lock().await.remove(&request_key);
            return Err(Error::internal_error().data(err.to_string()));
        }
    };
    if tx.send(json).await.is_err() {
        pending_requests.lock().await.remove(&request_key);
        return Err(Error::internal_error().data("WebSocket connection closed"));
    }

    Ok(PendingWsResponse {
        request_key,
        response_rx,
    })
}

async fn wait_for_websocket_response(
    pending: PendingWsResponse,
    pending_requests: PendingWsRequestMap,
    connection_cancel: CancellationToken,
    request_timeout: Duration,
) -> Result<serde_json::Value, Error> {
    let result = tokio::select! {
        _ = connection_cancel.cancelled() => {
            Err(Error::internal_error().data("WebSocket connection closed"))
        }
        response = tokio::time::timeout(request_timeout, pending.response_rx) => {
            match response {
                Ok(Ok(Ok(value))) => Ok(value),
                Ok(Ok(Err(error))) => Err(Error::internal_error().data(error)),
                Ok(Err(_)) => Err(Error::internal_error().data("WebSocket response channel dropped")),
                Err(_) => Err(Error::internal_error().data("WebSocket request timed out")),
            }
        }
    };
    pending_requests.lock().await.remove(&pending.request_key);
    result
}

pub(crate) async fn run_websocket_bridge(
    rx: mpsc::Receiver<ClientBridgeMessage>,
    tx: mpsc::Sender<String>,
    pending_requests: PendingWsRequestMap,
    request_counter: Arc<AtomicU64>,
    conn_id: String,
    connection_cancel: CancellationToken,
) {
    run_websocket_bridge_with_timeout(
        rx,
        tx,
        pending_requests,
        request_counter,
        conn_id,
        connection_cancel,
        WEBSOCKET_REQUEST_TIMEOUT,
    )
    .await;
}

pub(crate) async fn run_websocket_bridge_with_timeout(
    mut rx: mpsc::Receiver<ClientBridgeMessage>,
    tx: mpsc::Sender<String>,
    pending_requests: PendingWsRequestMap,
    request_counter: Arc<AtomicU64>,
    conn_id: String,
    connection_cancel: CancellationToken,
    request_timeout: Duration,
) {
    while let Some(message) = rx.recv().await {
        match message {
            ClientBridgeMessage::Notification(notification) => {
                if send_websocket_notification(&tx, "session/update", &notification)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            ClientBridgeMessage::Flush { response_tx } => {
                let _ = response_tx.send(Ok(()));
            }
            ClientBridgeMessage::ExtNotification(notification) => {
                if send_websocket_ext_notification(&tx, notification)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            ClientBridgeMessage::RequestPermission {
                request,
                response_tx,
            } => {
                match send_websocket_request(
                    &tx,
                    &pending_requests,
                    &request_counter,
                    &conn_id,
                    "session/request_permission",
                    &request,
                )
                .await
                {
                    Ok(pending) => {
                        let pending_requests = pending_requests.clone();
                        let connection_cancel = connection_cancel.clone();
                        tokio::spawn(async move {
                            let result = wait_for_websocket_response(
                                pending,
                                pending_requests,
                                connection_cancel,
                                request_timeout,
                            )
                            .await
                            .and_then(|value| {
                                serde_json::from_value::<RequestPermissionResponse>(value)
                                    .map_err(|err| Error::invalid_params().data(err.to_string()))
                            });
                            let _ = response_tx.send(result);
                        });
                    }
                    Err(error) => {
                        let _ = response_tx.send(Err(error));
                    }
                }
            }
            ClientBridgeMessage::Elicit {
                elicitation_id,
                session_id,
                message,
                requested_schema,
                source,
                response_tx,
            } => {
                let request = match create_elicitation_request(
                    elicitation_id,
                    session_id,
                    message,
                    requested_schema,
                    source,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        let _ = response_tx.send(Err(error));
                        continue;
                    }
                };
                match send_websocket_request(
                    &tx,
                    &pending_requests,
                    &request_counter,
                    &conn_id,
                    "elicitation/create",
                    &request,
                )
                .await
                {
                    Ok(pending) => {
                        let pending_requests = pending_requests.clone();
                        let connection_cancel = connection_cancel.clone();
                        tokio::spawn(async move {
                            let result = wait_for_websocket_response(
                                pending,
                                pending_requests,
                                connection_cancel,
                                request_timeout,
                            )
                            .await
                            .and_then(convert_elicitation_response_value);
                            let _ = response_tx.send(result);
                        });
                    }
                    Err(error) => {
                        let _ = response_tx.send(Err(error));
                    }
                }
            }
            ClientBridgeMessage::WorkspaceQuery { response_tx, .. } => {
                let _ = response_tx.send(Err(Error::method_not_found()));
            }
        }
    }
}

async fn send_websocket_notification<T: serde::Serialize>(
    tx: &mpsc::Sender<String>,
    method: &str,
    params: &T,
) -> Result<(), ()> {
    let wire = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    let json = serde_json::to_string(&wire).map_err(|_| ())?;
    tx.send(json).await.map_err(|_| ())
}

async fn send_websocket_ext_notification(
    tx: &mpsc::Sender<String>,
    notification: ExtNotification,
) -> Result<(), ()> {
    let params: serde_json::Value =
        serde_json::from_str(notification.params.get()).map_err(|_| ())?;
    let wire = serde_json::json!({
        "jsonrpc": "2.0",
        "method": notification.method,
        "params": params,
    });
    let json = serde_json::to_string(&wire).map_err(|_| ())?;
    tx.send(json).await.map_err(|_| ())
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
/// println!("Starting WebSocket ACP server on ws://127.0.0.1:3030/acp/ws");
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
    log::info!("WebSocket ACP server listening on ws://{}/acp/ws", addr);
    log::info!("Press Ctrl+C to stop");

    // Run with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown::signal())
        .await?;

    log::info!("WebSocket ACP server shutdown complete");
    Ok(())
}

/// WebSocket upgrade handler mounted at the canonical ACP endpoint.
pub(crate) fn router(agent: Arc<crate::agent::LocalAgentHandle>) -> Router {
    Router::new().nest("/acp", websocket_router(WsServerState::new(agent)))
}

#[cfg(feature = "dashboard")]
pub(crate) fn same_origin_router(agent: Arc<crate::agent::LocalAgentHandle>) -> Router {
    Router::new().nest(
        "/acp",
        websocket_router(WsServerState::new(agent))
            .route_layer(axum::middleware::from_fn(enforce_same_origin)),
    )
}

#[cfg(feature = "dashboard")]
async fn enforce_same_origin(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !has_allowed_websocket_origin(request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

fn websocket_router(state: WsServerState) -> Router {
    Router::new()
        .route("/ws", get(websocket_handler))
        .with_state(state)
}

async fn websocket_handler(ws: WebSocketUpgrade, State(state): State<WsServerState>) -> Response {
    ws.on_upgrade(|socket| handle_websocket_connection(socket, state))
        .into_response()
}

pub(crate) fn has_allowed_websocket_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        // Non-browser ACP clients do not send Origin.
        return true;
    };
    let (Some(origin), Some(host)) = (
        origin.to_str().ok(),
        headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok()),
    ) else {
        return false;
    };
    let Ok(origin) = origin.parse::<axum::http::Uri>() else {
        return false;
    };
    matches!(origin.scheme_str(), Some("http" | "https"))
        && origin
            .authority()
            .is_some_and(|authority| authority.as_str().eq_ignore_ascii_case(host))
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
    let (bridge_tx, bridge_rx) = mpsc::channel::<ClientBridgeMessage>(100);
    let session_bridge = ClientBridgeSender::for_connection(bridge_tx, conn_id.clone());
    state
        .connection_bridges
        .lock()
        .await
        .insert(conn_id.clone(), session_bridge.clone());

    spawn_event_forwarders(
        state.clone(),
        ConnectionEventState {
            conn_id: conn_id.clone(),
            tx: tx.clone(),
            pending_requests: pending_requests.clone(),
            forwarded_elicitations,
            request_counter: request_counter.clone(),
            connection_cancel: connection_cancel.clone(),
        },
    );

    let bridge_task = tokio::spawn(run_websocket_bridge(
        bridge_rx,
        tx.clone(),
        pending_requests.clone(),
        request_counter.clone(),
        conn_id.clone(),
        connection_cancel.clone(),
    ));

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
    let bridge_receive = session_bridge;
    let mut receive_task = tokio::spawn(async move {
        while let Some(result) = FuturesStreamExt::next(&mut ws_receiver).await {
            match result {
                Ok(Message::Text(text)) => match serde_json::from_str::<InboundWsMessage>(&text) {
                    Ok(InboundWsMessage::Request(request)) => {
                        tokio::spawn(dispatch_rpc_message_with_context(
                            RpcDispatchState {
                                agent: state_receive.agent.clone(),
                                session_owners: state_receive.session_owners.clone(),
                                pending_permissions: state_receive.pending_permissions.clone(),
                                pending_elicitations: state_receive.pending_elicitations.clone(),
                                conn_id: conn_id_receive.clone(),
                                tx: tx_receive.clone(),
                            },
                            request,
                            RpcDispatchContext {
                                session_hooks: None,
                                session_bridge: Some(bridge_receive.clone()),
                            },
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
    bridge_task.abort();

    cancel_pending_websocket_requests(&pending_requests).await;
    reconcile_websocket_disconnect(&state, &conn_id).await;
    log::info!("WebSocket connection closed: {}", conn_id);
}

async fn session_reconciliation_lock(state: &WsServerState, session_id: &str) -> Arc<Mutex<()>> {
    let mut locks = state.session_reconciliation_locks.lock().await;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(session_id).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(session_id.to_string(), Arc::downgrade(&lock));
    lock
}

pub(crate) async fn reconcile_websocket_disconnect(state: &WsServerState, conn_id: &str) {
    let connection_state = state
        .connection_bridges
        .lock()
        .await
        .remove(conn_id)
        .and_then(|bridge| bridge.connection_state());
    let _attachment_guard = match connection_state.as_ref() {
        Some(connection_state) => Some(connection_state.lock_attachment().await),
        None => None,
    };
    if let Some(connection_state) = connection_state.as_ref() {
        connection_state.deactivate();
    }

    let session_ids = state
        .session_owners
        .lock()
        .await
        .iter()
        .filter(|(_, subscribers)| subscribers.contains(conn_id))
        .map(|(session_id, _)| session_id.clone())
        .collect::<Vec<_>>();

    for session_id in session_ids {
        let session_lock = session_reconciliation_lock(state, &session_id).await;
        let guard = session_lock.lock().await;
        let fallback_bridge = {
            let live_bridges = state.connection_bridges.lock().await;
            let mut owners = state.session_owners.lock().await;
            let fallback = owners.get_mut(&session_id).and_then(|subscribers| {
                subscribers.remove(conn_id);
                subscribers
                    .iter()
                    .find_map(|subscriber| live_bridges.get(subscriber).cloned())
            });
            if owners.get(&session_id).is_some_and(HashSet::is_empty) {
                owners.remove(&session_id);
            }
            fallback
        };

        let cleared = state
            .agent
            .clear_session_bridge(&session_id, Arc::from(conn_id))
            .await;
        if cleared
            && let Some(fallback_bridge) = fallback_bridge
            && let Err(err) = state
                .agent
                .set_session_bridge(&session_id, fallback_bridge)
                .await
        {
            log::debug!("Failed to restore ACP bridge for session {session_id}: {err}");
        }
        drop(guard);
    }
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

fn spawn_global_notification_forwarders(state: WsServerState, connection: ConnectionEventState) {
    let mut notifications = state.agent.subscribe_ext_notifications();
    let tx = connection.tx.clone();
    let cancel = connection.connection_cancel.clone();
    tokio::spawn(async move {
        loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => break,
                result = notifications.recv() => match result {
                    Ok(notification) => notification,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            };
            if send_websocket_ext_notification(&tx, notification)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut model_updates = state.agent.model_inventory.subscribe_updates();
    let tx = connection.tx.clone();
    let cancel = connection.connection_cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = model_updates.recv() => match result {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
            let params = crate::control::notifications::ModelsChangedNotification {
                reason: "refresh_completed".to_string(),
            };
            if send_websocket_notification(
                &tx,
                crate::acp::shared::QMT_NOTIFICATION_MODELS_CHANGED,
                &params,
            )
            .await
            .is_err()
            {
                break;
            }
        }
    });

    #[cfg(feature = "remote")]
    if let Some(mesh) = state.agent.mesh() {
        let mut peer_events = mesh.subscribe_peer_events();
        let tx = connection.tx;
        let cancel = connection.connection_cancel;
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = cancel.cancelled() => break,
                    result = peer_events.recv() => match result {
                        Ok(event) => event,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    },
                };
                let result = match event {
                    crate::agent::remote::mesh::PeerEvent::Discovered(peer_id) => {
                        let params = crate::control::notifications::MeshNodesChangedNotification {
                            peer_id: peer_id.to_string(),
                            change: "discovered".to_string(),
                        };
                        send_websocket_notification(
                            &tx,
                            crate::acp::shared::QMT_NOTIFICATION_MESH_NODES_CHANGED,
                            &params,
                        )
                        .await
                    }
                    crate::agent::remote::mesh::PeerEvent::Expired(peer_id) => {
                        let params = crate::control::notifications::MeshPeerExpiredNotification {
                            peer_id: peer_id.to_string(),
                        };
                        send_websocket_notification(
                            &tx,
                            crate::acp::shared::QMT_NOTIFICATION_MESH_PEER_EXPIRED,
                            &params,
                        )
                        .await
                    }
                    _ => continue,
                };
                if result.is_err() {
                    break;
                }
            }
        });
    }
}

pub(crate) fn spawn_event_forwarders(state: WsServerState, connection: ConnectionEventState) {
    spawn_global_notification_forwarders(state.clone(), connection.clone());
    let translator = Arc::new(StdMutex::new(AcpLiveEventTranslator::new()));
    for event_source in &state.event_sources {
        let mut events = event_source.subscribe();
        let tx_events = connection.tx.clone();
        let conn_id_events = connection.conn_id.clone();
        let state_events = state.clone();
        let pending_events = connection.pending_requests.clone();
        let forwarded_events = connection.forwarded_elicitations.clone();
        let request_counter = connection.request_counter.clone();
        let connection_cancel = connection.connection_cancel.clone();
        let translator = translator.clone();

        tokio::spawn(async move {
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

                    let pending = match send_websocket_request(
                        &tx_events,
                        &pending_events,
                        &request_counter,
                        &conn_id_events,
                        "elicitation/create",
                        &request,
                    )
                    .await
                    {
                        Ok(pending) => pending,
                        Err(err) => {
                            log::warn!("Failed to send WebSocket elicitation: {err}");
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
                    };

                    let agent = state_events.agent.clone();
                    let session_id = session_id.clone();
                    let elicitation_id = elicitation_id.clone();
                    let pending_requests = pending_events.clone();
                    let cancel = connection_cancel.clone();
                    tokio::spawn(async move {
                        let response = match wait_for_websocket_response(
                            pending,
                            pending_requests,
                            cancel,
                            WEBSOCKET_REQUEST_TIMEOUT,
                        )
                        .await
                        {
                            Ok(value) => match convert_elicitation_response_value(value) {
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
                            Err(error) => {
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
                        };
                        resolve_websocket_elicitation(&agent, session_id, elicitation_id, response)
                            .await;
                    });
                    continue;
                }

                let notification = translator
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .translate_notification(&event);
                if let Some(notification) = notification {
                    let json = serde_json::to_string(&notification).unwrap_or_default();
                    if tx_events.send(json).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}
