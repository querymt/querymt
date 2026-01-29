//! WebSocket connection handling and event forwarding.
//!
//! Manages WebSocket lifecycle, message send/receive loops, and forwarding
//! agent events to connected clients.

use super::messages::{RoutingMode, UiClientMessage, UiServerMessage};
use super::session::{PRIMARY_AGENT_ID, build_agent_list};
use super::{ConnectionState, ServerState};
use crate::events::{AgentEvent, AgentEventKind};
use crate::session::domain::ForkOrigin;
use axum::extract::ws::{Message, WebSocket};
use futures_util::{sink::SinkExt, stream::StreamExt as FuturesStreamExt};
use std::collections::HashMap;
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
    let _ = send_message(
        tx,
        UiServerMessage::State {
            routing_mode,
            active_agent_id,
            active_session_id,
            agents,
            sessions_by_agent,
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
                        agent_id: agent_id.clone(),
                        event: event.clone(),
                    },
                )
                .await
                .is_err()
                {
                    break;
                }

                // Replay the child session's ProviderChanged event after SessionForked
                // registers ownership. The ProviderChanged was emitted during
                // new_session() before ownership was registered, so the event
                // forwarder missed it. This mirrors the replay in session.rs for
                // primary sessions.
                if let AgentEventKind::SessionForked {
                    child_session_id,
                    target_agent_id,
                    origin: ForkOrigin::Delegation,
                    ..
                } = &event.kind
                    && let Ok(audit) = state_events
                        .view_store
                        .get_audit_view(child_session_id)
                        .await
                    && let Some(provider_event) = audit
                        .events
                        .iter()
                        .rev()
                        .find(|e| matches!(e.kind, AgentEventKind::ProviderChanged { .. }))
                {
                    let _ = send_message(
                        &tx_events,
                        UiServerMessage::Event {
                            agent_id: target_agent_id.clone(),
                            event: provider_event.clone(),
                        },
                    )
                    .await;
                }
            }
        });
    }
}

/// Check if an event belongs to a connection's sessions.
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
