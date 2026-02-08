//! WebSocket connection handling and event forwarding.
//!
//! Manages WebSocket lifecycle, message send/receive loops, and forwarding
//! agent events to connected clients.

use super::messages::{RoutingMode, UiClientMessage, UiServerMessage};
use super::session::{PRIMARY_AGENT_ID, build_agent_list};
use super::{ConnectionState, ServerState};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{sink::SinkExt, stream::StreamExt as FuturesStreamExt};
use std::collections::{HashMap, HashSet};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Handle a new WebSocket connection.
pub async fn handle_websocket_connection(socket: WebSocket, state: ServerState) {
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
                subscribed_sessions: HashSet::new(),
                current_workspace_root: None,
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
                    super::handlers::handle_ui_message(
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

/// Send current state to a client.
pub async fn send_state(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    let (routing_mode, active_agent_id, active_session_id, sessions_by_agent) = {
        let connections = state.connections.lock().await;
        if let Some(conn) = connections.get(conn_id) {
            (
                conn.routing_mode,
                conn.active_agent_id.clone(),
                conn.sessions.get(&conn.active_agent_id).cloned(),
                conn.sessions.clone(),
            )
        } else {
            (
                RoutingMode::Single,
                PRIMARY_AGENT_ID.to_string(),
                None,
                HashMap::new(),
            )
        }
    };

    let agents = build_agent_list(state);
    let agent_mode = state.agent.get_agent_mode().as_str().to_string();
    let _ = send_message(
        tx,
        UiServerMessage::State {
            routing_mode,
            active_agent_id,
            active_session_id,
            agents,
            sessions_by_agent,
            agent_mode,
        },
    )
    .await;
}

/// Send a message to the client.
pub async fn send_message(
    tx: &mpsc::Sender<String>,
    message: UiServerMessage,
) -> Result<(), String> {
    let message_type = message.type_name();

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

/// Send an error message to the client.
pub async fn send_error(tx: &mpsc::Sender<String>, message: String) -> Result<(), String> {
    log::debug!("send_error: sending error message: {}", message);
    send_message(tx, UiServerMessage::Error { message }).await
}

/// Spawn event forwarders for all event sources.
pub fn spawn_event_forwarders(state: ServerState, conn_id: String, tx: mpsc::Sender<String>) {
    for event_source in &state.event_sources {
        let mut events = event_source.subscribe();
        let tx_events = tx.clone();
        let conn_id_events = conn_id.clone();
        let state_events = state.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                // Check if this connection is subscribed to this session
                let is_subscribed = {
                    let connections = state_events.connections.lock().await;
                    connections
                        .get(&conn_id_events)
                        .map(|conn| conn.subscribed_sessions.contains(&event.session_id))
                        .unwrap_or(false)
                };

                if !is_subscribed {
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
                        session_id: event.session_id.clone(),
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

/// Subscribe to file index updates for a workspace and forward them to the WebSocket client.
///
/// This spawns a long-lived task that listens to file index broadcasts from the FileIndexWatcher
/// and sends FileIndex messages to the client whenever files are created/modified/deleted.
pub async fn subscribe_to_file_index(
    state: ServerState,
    conn_id: String,
    tx: mpsc::Sender<String>,
    workspace_root: std::path::PathBuf,
) {
    // Get the workspace and subscribe to its file index updates
    let workspace = match state
        .workspace_manager
        .get_or_create(workspace_root.clone())
        .await
    {
        Ok(workspace) => workspace,
        Err(err) => {
            log::error!(
                "Failed to get workspace for file index subscription: {}",
                err
            );
            return;
        }
    };

    let mut index_rx = workspace.file_watcher().subscribe_index();

    // Update connection state to track which workspace we're subscribed to
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(&conn_id) {
            conn.current_workspace_root = Some(workspace_root.clone());
        }
    }

    log::info!(
        "Subscribed connection {} to file index updates for workspace {:?}",
        conn_id,
        workspace_root
    );

    // Spawn a task to forward file index updates to the client
    tokio::spawn(async move {
        while let Ok(index) = index_rx.recv().await {
            // Get the current session's cwd to filter the index appropriately
            let (_session_id, cwd) = {
                let connections = state.connections.lock().await;
                let conn = match connections.get(&conn_id) {
                    Some(conn) => conn,
                    None => {
                        // Connection closed, stop forwarding
                        log::debug!(
                            "Connection {} closed, stopping file index forwarding",
                            conn_id
                        );
                        break;
                    }
                };

                let session_id = conn.sessions.get(&conn.active_agent_id).cloned();

                let session_id = match session_id {
                    Some(id) => id,
                    None => continue, // No active session, skip this update
                };

                let cwds = state.session_cwds.lock().await;
                let cwd = match cwds.get(&session_id).cloned() {
                    Some(cwd) => cwd,
                    None => continue, // No cwd for this session, skip
                };

                (session_id, cwd)
            };

            // Filter the index to the session's cwd
            let relative_cwd = match cwd.strip_prefix(&workspace_root) {
                Ok(relative) => relative,
                Err(_) => continue, // cwd outside workspace root, skip
            };

            let files = super::mentions::filter_index_for_cwd(&index, relative_cwd);

            // Send the filtered index to the client
            if send_message(
                &tx,
                UiServerMessage::FileIndex {
                    files,
                    generated_at: index.generated_at,
                },
            )
            .await
            .is_err()
            {
                log::debug!(
                    "Failed to send file index to connection {}, stopping forwarding",
                    conn_id
                );
                break;
            }

            log::debug!("Pushed file index update to connection {}", conn_id);
        }
    });
}
