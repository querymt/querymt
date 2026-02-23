//! Standalone WebSocket ACP server.
//!
//! This module provides a pure ACP server over WebSocket without any dashboard UI.
//! It uses the same client bridge pattern as the stdio transport for consistency.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use querymt_agent::simple::Agent;
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

use crate::acp::client_bridge::ClientBridgeSender;
use crate::acp::shared::{
    PendingElicitationMap, PermissionMap, RpcRequest, SessionOwnerMap, collect_event_sources,
    handle_rpc_message, is_event_owned, translate_event_to_notification,
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
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

/// State for standalone WebSocket ACP server
#[derive(Clone)]
struct WsServerState {
    agent: Arc<crate::agent::AgentHandle>,
    pending_permissions: PermissionMap,
    pending_elicitations: PendingElicitationMap,
    event_sources: Vec<Arc<EventFanout>>,
    session_owners: SessionOwnerMap,
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
/// use querymt_agent::simple::Agent;
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
    agent: Arc<crate::agent::AgentHandle>,
    addr: &str,
) -> anyhow::Result<()> {
    log::info!("Starting standalone WebSocket ACP server on {}", addr);

    // Set up bridge for ClientBridgeSender pattern
    // Note: For WebSocket, the bridge is used for permission requests through the
    // pending_permissions map, not for direct client communication like in stdio
    let (tx, _rx) = mpsc::channel(100);
    let bridge_sender = ClientBridgeSender::new(tx);
    agent.set_bridge(bridge_sender);

    let event_sources = collect_event_sources(&agent);
    let pending_permissions = Arc::new(Mutex::new(HashMap::new()));
    let pending_elicitations = agent.pending_elicitations();
    let session_owners = Arc::new(Mutex::new(HashMap::new()));

    let state = WsServerState {
        agent,
        pending_permissions,
        pending_elicitations,
        event_sources,
        session_owners,
    };

    let app = Router::new()
        .route("/ws", get(websocket_handler))
        .with_state(state);

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

    // Spawn event forwarders to send agent events to this client
    spawn_event_forwarders(state.clone(), conn_id.clone(), tx.clone());

    // Task to send messages to the WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Clone values needed after the receive_task
    let conn_id_cleanup = conn_id.clone();
    let session_owners_cleanup = state.session_owners.clone();

    // Task to receive messages from the WebSocket
    let receive_task = tokio::spawn(async move {
        while let Some(result) = FuturesStreamExt::next(&mut ws_receiver).await {
            match result {
                Ok(Message::Text(text)) => match serde_json::from_str::<RpcRequest>(&text) {
                    Ok(request) => {
                        let response = handle_rpc_message(
                            state.agent.as_ref(),
                            &state.session_owners,
                            &state.pending_permissions,
                            &state.pending_elicitations,
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
                    log::info!("WebSocket closed by client: {}", conn_id);
                    break;
                }
                Ok(Message::Ping(_)) => {
                    log::trace!("Received ping from {}", conn_id);
                }
                Ok(_) => {}
                Err(e) => {
                    log::error!("WebSocket error for {}: {}", conn_id, e);
                    break;
                }
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = send_task => {},
        _ = receive_task => {},
    }

    // Clean up session ownership for this connection
    let mut owners = session_owners_cleanup.lock().await;
    owners.retain(|_, owner| owner != &conn_id_cleanup);

    log::info!("WebSocket connection closed: {}", conn_id_cleanup);
}

/// Spawn event forwarders that subscribe to event buses and forward events to the client.
///
/// For each event source (agent event bus), this spawns a task that:
/// 1. Subscribes to the event bus
/// 2. Filters events owned by this connection
/// 3. Translates events to JSON-RPC notifications
/// 4. Sends notifications to the client via the mpsc channel
fn spawn_event_forwarders(state: WsServerState, conn_id: String, tx: mpsc::Sender<String>) {
    for event_source in &state.event_sources {
        let mut events = event_source.subscribe();
        let tx_events = tx.clone();
        let conn_id_events = conn_id.clone();
        let state_events = state.clone();

        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                // Only forward events owned by this connection
                if !is_event_owned(&state_events.session_owners, &conn_id_events, &event).await {
                    continue;
                }

                // Translate and send event notification
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
