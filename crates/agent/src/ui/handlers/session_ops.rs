//! Session operation handlers.
//!
//! Covers the full lifecycle of a session visible from the UI:
//! listing, loading, cancelling, subscribing/unsubscribing, undo/redo,
//! elicitation responses, agent mode, file index, and LLM config queries.

use super::super::ServerState;
use super::super::connection::{send_error, send_message, send_state, subscribe_to_file_index};
use super::super::mentions::{filter_index_for_cwd, filter_index_for_cwd_entries};
use super::super::messages::{SessionGroup, SessionSummary, UiServerMessage};
use super::super::session::PRIMARY_AGENT_ID;
use crate::agent::core::AgentMode;
use crate::index::resolve_workspace_root;
use agent_client_protocol::CancelNotification;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

// ── Session list / load ───────────────────────────────────────────────────────

/// Handle session listing request.
pub async fn handle_list_sessions(state: &ServerState, tx: &mpsc::Sender<String>) {
    let view = match state.view_store.get_session_list_view(None).await {
        Ok(view) => view,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list sessions: {}", e)).await;
            return;
        }
    };

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
                    parent_session_id: s.parent_session_id,
                    fork_origin: s.fork_origin,
                    has_children: s.has_children,
                    node: None, // local sessions
                })
                .collect(),
        })
        .collect();

    let _ = send_message(tx, UiServerMessage::SessionList { groups }).await;
}

/// Handle session loading request.
pub async fn handle_load_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    // 1. Get audit view for this session only (child sessions loaded separately)
    let audit = match state.view_store.get_audit_view(session_id, false).await {
        Ok(audit) => audit,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to load session: {}", e)).await;
            return;
        }
    };

    // 1a. Load session to get cwd and populate session_cwds
    let cwd_path = if let Ok(Some(session)) =
        state.agent.config.provider.get_session(session_id).await
        && let Some(cwd) = session.cwd
    {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.to_string(), cwd.clone());
        Some(cwd)
    } else {
        None
    };

    // 2. Determine agent ID (default to primary)
    let agent_id = PRIMARY_AGENT_ID.to_string();

    // 3. Register in connection state
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.clone(), session_id.to_string());
            conn.subscribed_sessions.insert(session_id.to_string());
        }
    }

    // 4. Register agent mapping
    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id.clone());
    }

    // 5. Send loaded audit view and persisted undo stack for UI hydration
    let undo_stack = load_undo_stack(state, session_id).await;

    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            agent_id,
            audit,
            undo_stack,
        },
    )
    .await;

    // 6. Send updated state
    send_state(state, conn_id, tx).await;

    // 7. Subscribe to file index updates if this session has a cwd
    if let Some(cwd) = cwd_path {
        let root = resolve_workspace_root(&cwd);
        subscribe_to_file_index(state.clone(), conn_id.to_string(), tx.clone(), root).await;
    }
}

pub(super) async fn load_undo_stack(
    state: &ServerState,
    session_id: &str,
) -> Vec<super::super::messages::UndoStackFrame> {
    state
        .agent
        .config
        .provider
        .history_store()
        .list_revert_states(session_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|frame| super::super::messages::UndoStackFrame {
            message_id: frame.message_id,
        })
        .collect()
}

// ── Cancel ────────────────────────────────────────────────────────────────────

/// Handle session cancellation request.
pub async fn handle_cancel_session(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let Some(session_id) = session_id else {
        let _ = send_error(tx, "No active session to cancel".to_string()).await;
        return;
    };

    let agent_id = {
        let agents = state.session_agents.lock().await;
        agents.get(&session_id).cloned()
    };
    let agent = agent_id
        .and_then(|id| super::super::session::agent_for_id(state, &id))
        .unwrap_or_else(|| state.agent.clone());

    let notif = CancelNotification::new(session_id);
    if let Err(e) = agent.cancel(notif).await {
        let _ = send_error(tx, format!("Failed to cancel session: {}", e)).await;
    }
}

// ── Subscribe / Unsubscribe ───────────────────────────────────────────────────

/// Handle session subscription request with event replay.
pub async fn handle_subscribe_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    agent_id: Option<&str>,
    tx: &mpsc::Sender<String>,
) {
    // 1. Register subscription FIRST (so live events start flowing)
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.subscribed_sessions.insert(session_id.to_string());
        }
    }

    // 2. Register agent_id if provided
    if let Some(agent_id) = agent_id {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id.to_string());
    }

    // 3. Replay stored events (ViewStore has everything persisted)
    let (events, resolved_agent_id) = match state.view_store.get_audit_view(session_id, false).await
    {
        Ok(audit) => {
            let resolved_agent_id = {
                let agents = state.session_agents.lock().await;
                agents
                    .get(session_id)
                    .cloned()
                    .unwrap_or_else(|| PRIMARY_AGENT_ID.to_string())
            };
            (audit.events, resolved_agent_id)
        }
        Err(_) => {
            // Session may be brand new with no stored events yet — that's OK
            (
                vec![],
                agent_id
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| PRIMARY_AGENT_ID.to_string()),
            )
        }
    };

    // 4. Send replay batch
    let _ = send_message(
        tx,
        UiServerMessage::SessionEvents {
            session_id: session_id.to_string(),
            agent_id: resolved_agent_id,
            events,
        },
    )
    .await;
}

/// Handle session unsubscription request.
pub async fn handle_unsubscribe_session(state: &ServerState, conn_id: &str, session_id: &str) {
    let mut connections = state.connections.lock().await;
    if let Some(conn) = connections.get_mut(conn_id) {
        conn.subscribed_sessions.remove(session_id);
    }
}

// ── Undo / Redo ───────────────────────────────────────────────────────────────

/// Handle undo request.
pub async fn handle_undo(
    state: &ServerState,
    conn_id: &str,
    message_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let Some(session_id) = session_id else {
        let _ = send_error(tx, "No active session".to_string()).await;
        return;
    };

    match state.agent.undo(&session_id, message_id).await {
        Ok(result) => {
            let undo_stack = load_undo_stack(state, &session_id).await;
            let _ = send_message(
                tx,
                UiServerMessage::UndoResult {
                    success: true,
                    message: None,
                    reverted_files: result.reverted_files,
                    message_id: Some(result.message_id),
                    undo_stack,
                },
            )
            .await;
        }
        Err(e) => {
            let undo_stack = load_undo_stack(state, &session_id).await;
            let _ = send_message(
                tx,
                UiServerMessage::UndoResult {
                    success: false,
                    message: Some(e.to_string()),
                    reverted_files: vec![],
                    message_id: None,
                    undo_stack,
                },
            )
            .await;
        }
    }
}

/// Handle redo request.
pub async fn handle_redo(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let Some(session_id) = session_id else {
        let _ = send_error(tx, "No active session".to_string()).await;
        return;
    };

    match state.agent.redo(&session_id).await {
        Ok(_result) => {
            let undo_stack = load_undo_stack(state, &session_id).await;
            let _ = send_message(
                tx,
                UiServerMessage::RedoResult {
                    success: true,
                    message: None,
                    undo_stack,
                },
            )
            .await;
        }
        Err(e) => {
            let undo_stack = load_undo_stack(state, &session_id).await;
            let _ = send_message(
                tx,
                UiServerMessage::RedoResult {
                    success: false,
                    message: Some(e.to_string()),
                    undo_stack,
                },
            )
            .await;
        }
    }
}

// ── Elicitation ───────────────────────────────────────────────────────────────

/// Handle elicitation response from UI client.
pub async fn handle_elicitation_response(
    state: &ServerState,
    elicitation_id: &str,
    action: &str,
    content: Option<&serde_json::Value>,
) {
    let action_enum = match action {
        "accept" => crate::elicitation::ElicitationAction::Accept,
        "decline" => crate::elicitation::ElicitationAction::Decline,
        "cancel" => crate::elicitation::ElicitationAction::Cancel,
        _ => {
            log::warn!("Invalid elicitation action: {}", action);
            return;
        }
    };

    let response = crate::elicitation::ElicitationResponse {
        action: action_enum,
        content: content.cloned(),
    };

    let pending_map = state.agent.pending_elicitations();
    let mut pending = pending_map.lock().await;
    if let Some(tx) = pending.remove(elicitation_id) {
        let _ = tx.send(response);
    }
}

// ── Agent mode ────────────────────────────────────────────────────────────────

/// Handle agent mode change request.
///
/// Sets the mode on the active session actor (per-session mode) and updates
/// the default mode for new sessions.
pub async fn handle_set_agent_mode(
    state: &ServerState,
    conn_id: &str,
    mode: &str,
    tx: &mpsc::Sender<String>,
) {
    match mode.parse::<AgentMode>() {
        Ok(new_mode) => {
            let session_id = {
                let connections = state.connections.lock().await;
                connections
                    .get(conn_id)
                    .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
            };

            let previous_mode = if let Some(ref session_id) = session_id {
                let registry = state.agent.registry.lock().await;
                if let Some(session_ref) = registry.get(session_id) {
                    session_ref.get_mode().await.ok()
                } else {
                    None
                }
            } else {
                None
            }
            .unwrap_or_else(|| {
                state
                    .agent
                    .default_mode
                    .lock()
                    .map(|m| *m)
                    .unwrap_or(AgentMode::Build)
            });

            if let Ok(mut default_mode) = state.agent.default_mode.lock() {
                *default_mode = new_mode;
            }

            let mode_set_on_actor = if let Some(ref session_id) = session_id {
                let registry = state.agent.registry.lock().await;
                if let Some(session_ref) = registry.get(session_id) {
                    match session_ref.set_mode(new_mode).await {
                        Ok(_) => {
                            log::info!(
                                "Agent mode changed on session {}: {} -> {}",
                                session_id,
                                previous_mode,
                                new_mode
                            );
                            true
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to set mode on session {}: {}. Mode will apply to next session.",
                                session_id,
                                e
                            );
                            false
                        }
                    }
                } else {
                    log::debug!(
                        "No active session actor for {}. Mode will apply to next session.",
                        session_id
                    );
                    false
                }
            } else {
                log::debug!("No active session. Mode will apply to next session.");
                false
            };

            if !mode_set_on_actor {
                log::info!(
                    "Default agent mode changed: {} -> {} (will apply to next session)",
                    previous_mode,
                    new_mode
                );
            }

            let _ = send_message(
                tx,
                UiServerMessage::AgentMode {
                    mode: new_mode.as_str().to_string(),
                },
            )
            .await;
        }
        Err(e) => {
            let _ = send_error(tx, e).await;
        }
    }
}

/// Handle get agent mode request.
///
/// Reads mode from the active session actor if available, otherwise from the
/// default mode.
pub async fn handle_get_agent_mode(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let mode = if let Some(session_id) = session_id {
        let registry = state.agent.registry.lock().await;
        if let Some(session_ref) = registry.get(&session_id) {
            match session_ref.get_mode().await {
                Ok(m) => m,
                Err(_) => state
                    .agent
                    .default_mode
                    .lock()
                    .map(|m| *m)
                    .unwrap_or(AgentMode::Build),
            }
        } else {
            state
                .agent
                .default_mode
                .lock()
                .map(|m| *m)
                .unwrap_or(AgentMode::Build)
        }
    } else {
        state
            .agent
            .default_mode
            .lock()
            .map(|m| *m)
            .unwrap_or(AgentMode::Build)
    };

    let _ = send_message(
        tx,
        UiServerMessage::AgentMode {
            mode: mode.as_str().to_string(),
        },
    )
    .await;
}

// ── File index / LLM config ───────────────────────────────────────────────────

/// Handle file index request.
pub async fn handle_get_file_index(state: &ServerState, conn_id: &str, tx: &mpsc::Sender<String>) {
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let Some(session_id) = session_id else {
        let _ = send_error(tx, "No active session".to_string()).await;
        return;
    };

    // For remote sessions, proxy the file index request to the remote SessionActor.
    {
        let registry = state.agent.registry.lock().await;
        if let Some(actor_ref) = registry.get(&session_id)
            && actor_ref.is_remote()
        {
            let actor_ref = actor_ref.clone();
            drop(registry);

            let cwd = {
                let cwds = state.session_cwds.lock().await;
                cwds.get(&session_id).cloned()
            };

            match actor_ref.get_file_index().await {
                Ok(resp) => {
                    let root = std::path::PathBuf::from(&resp.workspace_root);
                    let files = match cwd.as_ref().and_then(|c| c.strip_prefix(&root).ok()) {
                        Some(relative_cwd) => {
                            filter_index_for_cwd_entries(&resp.files, relative_cwd)
                        }
                        None => resp.files,
                    };
                    let _ = send_message(
                        tx,
                        UiServerMessage::FileIndex {
                            files,
                            generated_at: resp.generated_at,
                        },
                    )
                    .await;
                }
                Err(e) => {
                    let _ = send_error(tx, format!("Remote file index: {e}")).await;
                }
            }
            return;
        }
    }

    let cwd = {
        let cwds = state.session_cwds.lock().await;
        cwds.get(&session_id).cloned()
    };

    let Some(cwd) = cwd else {
        let _ = send_error(tx, "No working directory set for this session".to_string()).await;
        return;
    };

    let root = resolve_workspace_root(&cwd);

    let workspace = match state
        .workspace_manager
        .ask(crate::index::GetOrCreate { root: root.clone() })
        .await
    {
        Ok(handle) => handle,
        Err(err) => {
            let _ = send_error(tx, format!("Workspace index error: {}", err)).await;
            return;
        }
    };

    let Some(index) = workspace.file_index() else {
        let _ = send_error(tx, "File index not ready".to_string()).await;
        return;
    };

    let relative_cwd = match cwd.strip_prefix(&root) {
        Ok(relative) => relative,
        Err(_) => {
            let _ = send_error(tx, "Working directory outside workspace root".to_string()).await;
            return;
        }
    };

    let files = filter_index_for_cwd(&index, relative_cwd);

    let _ = send_message(
        tx,
        UiServerMessage::FileIndex {
            files,
            generated_at: index.generated_at,
        },
    )
    .await;
}

/// Handle LLM config request.
pub async fn handle_get_llm_config(state: &ServerState, config_id: i64, tx: &mpsc::Sender<String>) {
    match state.agent.get_llm_config(config_id).await {
        Ok(Some(config)) => {
            let _ = send_message(
                tx,
                UiServerMessage::LlmConfig {
                    config_id: config.id,
                    provider: config.provider,
                    model: config.model,
                    params: config.params,
                },
            )
            .await;
        }
        Ok(None) => {
            let _ = send_error(tx, format!("LLM config not found: {}", config_id)).await;
        }
        Err(err) => {
            let _ = send_error(tx, format!("Failed to get LLM config: {}", err)).await;
        }
    }
}
