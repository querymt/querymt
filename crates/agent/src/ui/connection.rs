//! WebSocket connection handling and event forwarding.
//!
//! Manages WebSocket lifecycle, message send/receive loops, and forwarding
//! agent events to connected clients.

use crate::agent::core::AgentMode;

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
    #[cfg(feature = "remote")]
    spawn_peer_event_watcher(state.clone(), tx.clone());
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

    {
        let mut connections = state.connections.lock().await;
        connections.remove(&conn_id);
    }

    super::handlers::stop_oauth_callback_listener_for_connection(&state, &conn_id).await;

    {
        let mut oauth_flows = state.oauth_flows.lock().await;
        oauth_flows.retain(|_, flow| flow.conn_id != conn_id);
    }
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
    let default_cwd = state
        .default_cwd
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());

    // Try to get mode from active session actor, fall back to default_mode
    let agent_mode = if let Some(ref session_id) = active_session_id {
        let registry = state.agent.registry.lock().await;
        if let Some(session_ref) = registry.get(session_id) {
            match session_ref.get_mode().await {
                Ok(m) => m,
                Err(_) => state
                    .agent
                    .default_mode
                    .lock()
                    .map(|guard| *guard)
                    .unwrap_or(AgentMode::Build),
            }
        } else {
            state
                .agent
                .default_mode
                .lock()
                .map(|guard| *guard)
                .unwrap_or(AgentMode::Build)
        }
    } else {
        state
            .agent
            .default_mode
            .lock()
            .map(|guard| *guard)
            .unwrap_or(AgentMode::Build)
    };

    let _ = send_message(
        tx,
        UiServerMessage::State {
            routing_mode,
            active_agent_id,
            active_session_id,
            default_cwd,
            agents,
            sessions_by_agent,
            agent_mode: agent_mode.as_str().to_string(),
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

/// Spawn a task that watches for mDNS peer discovery / expiry events and pushes
/// an updated `remote_nodes` list to this WebSocket client whenever the mesh
/// topology changes.
///
/// This replaces polling: the kameo swarm event loop emits a `PeerEvent` the
/// instant mDNS fires, and we react immediately by re-querying the DHT and
/// pushing the fresh node list to the client.
#[cfg(feature = "remote")]
pub fn spawn_peer_event_watcher(state: ServerState, tx: mpsc::Sender<String>) {
    use super::messages::RemoteNodeInfo;
    use crate::agent::remote::PeerEvent;

    let Some(mesh) = state.agent.mesh() else {
        return;
    };

    let mut rx = mesh.subscribe_peer_events();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if tx.is_closed() {
                        break;
                    }

                    // On Expired we can push the update immediately because
                    // the node is gone and the DHT doesn't need time to settle.
                    // On Discovered we use a retry loop: the remote node needs
                    // time to bootstrap and register its actors in the DHT, so
                    // we poll with exponential back-off rather than a fixed sleep.
                    let needs_retry = match event {
                        PeerEvent::Discovered(peer_id) => {
                            log::debug!(
                                "spawn_peer_event_watcher: peer discovered {peer_id}, will poll until actor is visible"
                            );
                            true
                        }
                        PeerEvent::Expired(peer_id) => {
                            log::debug!(
                                "spawn_peer_event_watcher: peer expired {peer_id}, refreshing remote nodes"
                            );
                            false
                        }
                    };

                    if tx.is_closed() {
                        break;
                    }

                    if needs_retry {
                        // Exponential back-off: 500 ms, 1 s, 2 s, 4 s, 8 s.
                        // We stop as soon as the newly discovered peer shows up
                        // in list_remote_nodes(), giving up after ~15 s total.
                        const DELAYS_MS: &[u64] = &[500, 1_000, 2_000, 4_000, 8_000];
                        let mut pushed = false;
                        for &delay_ms in DELAYS_MS {
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

                            if tx.is_closed() {
                                break;
                            }

                            let nodes = state.agent.list_remote_nodes().await;
                            let msg = UiServerMessage::RemoteNodes {
                                nodes: nodes
                                    .iter()
                                    .map(|n| RemoteNodeInfo {
                                        label: n.hostname.clone(),
                                        capabilities: n.capabilities.clone(),
                                        active_sessions: n.active_sessions,
                                    })
                                    .collect(),
                            };

                            if send_message(&tx, msg).await.is_err() {
                                break;
                            }

                            // Stop retrying once we can see at least one remote node
                            // — the UI has been updated and the peer is reachable.
                            if !nodes.is_empty() {
                                pushed = true;
                                break;
                            }

                            log::debug!(
                                "spawn_peer_event_watcher: no remote nodes yet, retrying in {}ms",
                                delay_ms * 2
                            );
                        }

                        if !pushed {
                            log::warn!(
                                "spawn_peer_event_watcher: peer discovered but no remote nodes \
                                 visible after all retries — peer may not have registered its actors yet"
                            );
                        }
                    } else {
                        // Expired: push the updated (node-removed) list immediately.
                        let nodes = state.agent.list_remote_nodes().await;
                        let msg = UiServerMessage::RemoteNodes {
                            nodes: nodes
                                .into_iter()
                                .map(|n| RemoteNodeInfo {
                                    label: n.hostname,
                                    capabilities: n.capabilities,
                                    active_sessions: n.active_sessions,
                                })
                                .collect(),
                        };

                        if send_message(&tx, msg).await.is_err() {
                            break;
                        }

                        // Schedule a delayed re-check to catch any stale DHT
                        // records that resolved between the immediate push and
                        // the DHT eventually purging the expired peer.
                        let state_delayed = state.clone();
                        let tx_delayed = tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            if tx_delayed.is_closed() {
                                return;
                            }
                            let nodes = state_delayed.agent.list_remote_nodes().await;
                            let msg = UiServerMessage::RemoteNodes {
                                nodes: nodes
                                    .into_iter()
                                    .map(|n| RemoteNodeInfo {
                                        label: n.hostname,
                                        capabilities: n.capabilities,
                                        active_sessions: n.active_sessions,
                                    })
                                    .collect(),
                            };
                            let _ = send_message(&tx_delayed, msg).await;
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // We missed some events — re-query immediately to catch up.
                    log::warn!("spawn_peer_event_watcher: lagged by {n} events, re-querying");
                    let nodes = state.agent.list_remote_nodes().await;
                    let msg = UiServerMessage::RemoteNodes {
                        nodes: nodes
                            .into_iter()
                            .map(|n| RemoteNodeInfo {
                                label: n.hostname,
                                capabilities: n.capabilities,
                                active_sessions: n.active_sessions,
                            })
                            .collect(),
                    };
                    if send_message(&tx, msg).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    // The swarm is shutting down — nothing to do.
                    break;
                }
            }
        }
    });
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
    let handle = match state
        .workspace_manager
        .ask(crate::index::GetOrCreate {
            root: workspace_root.clone(),
        })
        .await
    {
        Ok(handle) => handle,
        Err(err) => {
            log::error!(
                "Failed to get workspace for file index subscription: {}",
                err
            );
            return;
        }
    };

    let mut index_rx = handle.file_watcher.subscribe_index();

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

    // IMPORTANT: Send the current index immediately to avoid subscription race condition
    // When subscribing to a broadcast channel, we only receive FUTURE messages.
    // If the workspace already existed (cached), the initial index was sent before we subscribed.
    // This ensures new subscribers get the current state immediately.
    if let Some(current_index) = handle.file_index() {
        // Get current session's cwd to filter the index
        let cwd = {
            let connections = state.connections.lock().await;
            let conn = connections.get(&conn_id);
            let session_id = conn.and_then(|c| c.sessions.get(&c.active_agent_id).cloned());

            if let Some(session_id) = session_id {
                let cwds = state.session_cwds.lock().await;
                cwds.get(&session_id).cloned()
            } else {
                None
            }
        };

        if let Some(cwd) = cwd
            && let Ok(relative_cwd) = cwd.strip_prefix(&workspace_root)
        {
            let files = super::mentions::filter_index_for_cwd(&current_index, relative_cwd);

            // Send initial index
            if let Err(err) = send_message(
                &tx,
                UiServerMessage::FileIndex {
                    files,
                    generated_at: current_index.generated_at,
                },
            )
            .await
            {
                log::warn!(
                    "Failed to send initial file index to connection {}: {}",
                    conn_id,
                    err
                );
            } else {
                log::debug!("Sent initial file index to connection {}", conn_id);
            }
        }
    }

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
