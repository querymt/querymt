//! Session operation handlers.
//!
//! Covers the full lifecycle of a session visible from the UI:
//! listing, loading, cancelling, subscribing/unsubscribing, undo/redo,
//! elicitation responses, agent mode, file index, and LLM config queries.

use super::super::ServerState;
use super::super::connection::{send_error, send_message, send_state, subscribe_to_file_index};
use super::super::mentions::{filter_index_for_cwd, filter_index_for_cwd_entries};
use super::super::messages::{SessionGroup, SessionSummary, UiServerMessage};
use super::super::{cursor_from_events, session::PRIMARY_AGENT_ID};
use crate::agent::core::AgentMode;
use crate::events::EventEnvelope;
use crate::index::resolve_workspace_root;
use crate::send_agent::SendAgent;
use crate::session::domain::ForkOrigin;
use agent_client_protocol::{CancelNotification, LoadSessionRequest, SessionId};
use querymt::chat::ReasoningEffort;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tracing::Instrument;

// ── Session list / load ───────────────────────────────────────────────────────

/// Handle session listing request.
#[tracing::instrument(
    name = "ui.handle_list_sessions",
    skip(state, tx),
    fields(
        local_group_count = tracing::field::Empty,
        local_session_count = tracing::field::Empty,
        remote_group_count = tracing::field::Empty,
        remote_session_count = tracing::field::Empty,
        total_group_count = tracing::field::Empty,
        total_session_count = tracing::field::Empty,
        view_fetch_ms = tracing::field::Empty,
        remote_merge_ms = tracing::field::Empty,
        total_ms = tracing::field::Empty
    )
)]
pub async fn handle_list_sessions(state: &ServerState, tx: &mpsc::Sender<String>) {
    let started = Instant::now();
    let view_started = Instant::now();

    let view = match state
        .view_store
        .get_session_list_view(None)
        .instrument(tracing::info_span!(
            "ui.handle_list_sessions.get_session_list_view"
        ))
        .await
    {
        Ok(view) => view,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list sessions: {}", e)).await;
            return;
        }
    };

    let view_fetch_ms = view_started.elapsed().as_millis() as u64;
    let local_group_count = view.groups.len();
    let local_session_count: usize = view.groups.iter().map(|g| g.sessions.len()).sum();

    let mut groups: Vec<SessionGroup> = view
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
                    session_kind: s.session_kind,
                    has_children: s.has_children,
                    node: None,     // local sessions have no node label
                    node_id: None,  // not applicable for local sessions
                    attached: None, // not applicable for local sessions
                })
                .collect(),
        })
        .collect();

    // Append in-memory remote sessions (not persisted to the local view store).
    // Group them by peer_label so each remote node gets its own collapsible group.
    #[cfg(feature = "remote")]
    let mut remote_group_count = 0usize;
    #[cfg(feature = "remote")]
    let mut remote_session_count = 0usize;
    #[cfg(not(feature = "remote"))]
    let remote_group_count = 0usize;
    #[cfg(not(feature = "remote"))]
    let remote_session_count = 0usize;

    let remote_merge_started = Instant::now();
    #[cfg(feature = "remote")]
    {
        async {
            // 1. Collect already-attached remote sessions from the registry.
            let attached_sessions: std::collections::HashSet<String>;
            let remote = {
                let registry = state.agent.registry.lock().await;
                let sessions = registry.remote_sessions();
                attached_sessions = sessions.iter().map(|(id, _)| id.clone()).collect();
                sessions
            };

            // Collect per-node groups: node_label -> Vec<SessionSummary>
            let mut by_node: std::collections::HashMap<String, Vec<SessionSummary>> =
                std::collections::HashMap::new();

            // Build a peer_label -> node_id_str map from live remote nodes so we
            // can populate SessionSummary::node_id for both attached and discovered
            // sessions.  The frontend needs node_id to send attach_remote_session.
            let node_id_by_label: std::collections::HashMap<String, String> =
                if state.agent.mesh().is_some() {
                    state
                        .agent
                        .list_remote_nodes()
                        .await
                        .into_iter()
                        .map(|n| (n.hostname, n.node_id.to_string()))
                        .collect()
                } else {
                    std::collections::HashMap::new()
                };

            if !remote.is_empty() {
                let cwds = state.session_cwds.lock().await;

                for (session_id, peer_label) in remote {
                    let cwd = cwds.get(&session_id).map(|p| p.display().to_string());
                    let node_id = node_id_by_label.get(&peer_label).cloned();
                    by_node
                        .entry(peer_label.clone())
                        .or_default()
                        .push(SessionSummary {
                            session_id,
                            name: None,
                            cwd: cwd.clone(),
                            title: None,
                            created_at: None,
                            updated_at: None,
                            parent_session_id: None,
                            fork_origin: None,
                            session_kind: None,
                            has_children: false,
                            node: Some(peer_label),
                            node_id,
                            attached: Some(true), // in-memory remote sessions are attached
                        });
                }
            }

            // 2. Query each live peer for their sessions and include
            //    unattached ones so the UI can show "available" remote
            //    sessions after restart (Bug 2 fix).
            //    Peers are queried in parallel to avoid accumulating the
            //    2-second timeout for each peer serially.
            if state.agent.mesh().is_some() {
                let peer_futures: Vec<_> = node_id_by_label
                    .iter()
                    .map(|(peer_label, node_id_str)| {
                        let peer_label = peer_label.clone();
                        let node_id_str = node_id_str.clone();
                        let state = state.clone();
                        async move {
                            let sessions = match tokio::time::timeout(
                                std::time::Duration::from_secs(2),
                                async {
                                    let nm_ref =
                                        state.agent.find_node_manager(&node_id_str).await?;
                                    state.agent.list_remote_sessions(&nm_ref).await
                                },
                            )
                            .await
                            {
                                Ok(Ok(s)) => s,
                                Ok(Err(e)) => {
                                    log::debug!(
                                        "handle_list_sessions: failed to query sessions from {}: {}",
                                        peer_label,
                                        e.message
                                    );
                                    return None;
                                }
                                Err(_) => {
                                    log::debug!(
                                        "handle_list_sessions: timeout querying sessions from {}",
                                        peer_label
                                    );
                                    return None;
                                }
                            };
                            Some((peer_label, node_id_str, sessions))
                        }
                    })
                    .collect();

                let peer_results =
                    futures_util::future::join_all(peer_futures).await;

                for result in peer_results.into_iter().flatten() {
                    let (peer_label, node_id_str, sessions) = result;
                    for session_info in sessions {
                        // Skip sessions that are already attached.
                        if attached_sessions.contains(&session_info.session_id) {
                            continue;
                        }

                        by_node
                            .entry(peer_label.clone())
                            .or_default()
                            .push(SessionSummary {
                                session_id: session_info.session_id,
                                name: None,
                                cwd: session_info.cwd,
                                title: None,
                                created_at: None,
                                updated_at: None,
                                parent_session_id: None,
                                fork_origin: None,
                                session_kind: None,
                                has_children: false,
                                node: Some(peer_label.clone()),
                                node_id: Some(node_id_str.clone()),
                                attached: Some(false), // discovered but not attached
                            });
                    }
                }
            }

            for (node_label, sessions) in by_node {
                remote_group_count += 1;
                remote_session_count += sessions.len();
                // Use a synthetic cwd like "remote::<node>" so the group header
                // is recognisable without requiring a real path.
                groups.push(SessionGroup {
                    cwd: Some(format!("remote::{}", node_label)),
                    sessions,
                    latest_activity: None,
                });
            }
        }
        .instrument(tracing::info_span!("ui.handle_list_sessions.remote_merge"))
        .await;
    }
    let remote_merge_ms = remote_merge_started.elapsed().as_millis() as u64;

    let total_group_count = groups.len();
    let total_session_count: usize = groups.iter().map(|g| g.sessions.len()).sum();

    let span = tracing::Span::current();
    span.record("local_group_count", local_group_count);
    span.record("local_session_count", local_session_count);
    span.record("remote_group_count", remote_group_count);
    span.record("remote_session_count", remote_session_count);
    span.record("total_group_count", total_group_count);
    span.record("total_session_count", total_session_count);
    span.record("view_fetch_ms", view_fetch_ms);
    span.record("remote_merge_ms", remote_merge_ms);
    span.record("total_ms", started.elapsed().as_millis() as u64);

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

    if let Err(e) = ensure_session_loaded(state, session_id, "load_session").await {
        let _ = send_error(tx, e).await;
        return;
    }

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
    let cursor = cursor_from_events(&audit.events);

    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            agent_id,
            audit,
            undo_stack,
            cursor: cursor.clone(),
        },
    )
    .await;

    // 6. Seed per-connection stream cursor from replay tail
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.session_cursors
                .insert(session_id.to_string(), cursor.clone());
        }
    }

    // 7. Send updated state
    send_state(state, conn_id, tx).await;

    // 7. Subscribe to file index updates if this session has a cwd
    if let Some(cwd) = cwd_path {
        let root = resolve_workspace_root(&cwd);
        subscribe_to_file_index(state.clone(), conn_id.to_string(), tx.clone(), root).await;
    }
}

/// Handle session deletion request.
pub async fn handle_delete_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    if let Err(err) = state.session_store.delete_session(session_id).await {
        let _ = send_error(tx, format!("Failed to delete session: {}", err)).await;
        return;
    }

    {
        let mut registry = state.agent.registry.lock().await;
        // For remote sessions, send UnsubscribeEvents before removing so the
        // remote EventForwarder task is properly cleaned up.
        #[cfg(feature = "remote")]
        {
            registry.detach_remote_session(session_id).await;
        }
        #[cfg(not(feature = "remote"))]
        {
            registry.remove(session_id);
        }
    }

    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions.retain(|_, sid| sid != session_id);
            conn.subscribed_sessions.remove(session_id);
            conn.session_cursors.remove(session_id);
        }
    }

    {
        let mut agents = state.session_agents.lock().await;
        agents.remove(session_id);
    }

    {
        let mut cwds = state.session_cwds.lock().await;
        cwds.remove(session_id);
    }

    send_state(state, conn_id, tx).await;
    handle_list_sessions(state, tx).await;
}

pub(super) async fn ensure_session_loaded(
    state: &ServerState,
    session_id: &str,
    op_name: &'static str,
) -> Result<(), String> {
    let registry_hit = {
        let registry = state.agent.registry.lock().await;
        registry.get(session_id).is_some()
    };

    if registry_hit {
        tracing::debug!(
            op_name,
            session_id,
            registry_hit,
            store_exists = true,
            actor_loaded = true,
            "session already hydrated"
        );
        return Ok(());
    }

    let stored_session = state
        .agent
        .config
        .provider
        .history_store()
        .get_session(session_id)
        .await
        .map_err(|e| e.to_string())?;

    let Some(session) = stored_session else {
        tracing::warn!(
            op_name,
            session_id,
            registry_hit,
            store_exists = false,
            actor_loaded = false,
            "session hydration failed: missing from store"
        );
        return Err(format!("Session not found: {}", session_id));
    };

    if let Some(cwd) = session.cwd.clone() {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.to_string(), cwd);
    }

    let store_exists = true;
    let req = LoadSessionRequest::new(
        SessionId::from(session_id.to_string()),
        session.cwd.unwrap_or_else(PathBuf::new),
    );
    state
        .agent
        .load_session(req)
        .await
        .map_err(|e| e.to_string())?;

    let actor_loaded = {
        let registry = state.agent.registry.lock().await;
        registry.get(session_id).is_some()
    };

    tracing::info!(
        op_name,
        session_id,
        registry_hit,
        store_exists,
        actor_loaded,
        "session lazy hydration evaluated"
    );

    if actor_loaded {
        Ok(())
    } else {
        Err(format!("Session not found: {}", session_id))
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
        .unwrap_or_else(|| state.agent.clone() as Arc<dyn crate::agent::handle::AgentHandle>);

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

    let cursor = cursor_from_events(&events);
    let events: Vec<EventEnvelope> = events.into_iter().map(Into::into).collect();

    // 4. Track replay cursor and send replay batch
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.session_cursors
                .insert(session_id.to_string(), cursor.clone());
        }
    }

    let _ = send_message(
        tx,
        UiServerMessage::SessionEvents {
            session_id: session_id.to_string(),
            agent_id: resolved_agent_id,
            events,
            cursor,
        },
    )
    .await;
}

/// Handle session unsubscription request.
pub async fn handle_unsubscribe_session(state: &ServerState, conn_id: &str, session_id: &str) {
    let mut connections = state.connections.lock().await;
    if let Some(conn) = connections.get_mut(conn_id) {
        conn.subscribed_sessions.remove(session_id);
        conn.session_cursors.remove(session_id);
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

/// Handle fork request from the active session at a specific message boundary.
pub async fn handle_fork_session(
    state: &ServerState,
    conn_id: &str,
    message_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let source_session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let Some(source_session_id) = source_session_id else {
        let _ = send_message(
            tx,
            UiServerMessage::ForkResult {
                success: false,
                source_session_id: None,
                forked_session_id: None,
                message: Some("No active session".to_string()),
            },
        )
        .await;
        return;
    };

    match state
        .session_store
        .fork_session(&source_session_id, message_id, ForkOrigin::User)
        .await
    {
        Ok(forked_session_id) => {
            if let Ok(Some(forked_session)) =
                state.session_store.get_session(&forked_session_id).await
                && let Some(cwd) = forked_session.cwd
            {
                let mut cwds = state.session_cwds.lock().await;
                cwds.insert(forked_session_id.clone(), cwd);
            }

            {
                let mut connections = state.connections.lock().await;
                if let Some(conn) = connections.get_mut(conn_id) {
                    conn.sessions
                        .insert(conn.active_agent_id.clone(), forked_session_id.clone());
                    conn.subscribed_sessions.insert(forked_session_id.clone());
                }
            }

            {
                let mut agents = state.session_agents.lock().await;
                let fallback_agent = PRIMARY_AGENT_ID.to_string();
                let parent_agent = agents
                    .get(&source_session_id)
                    .cloned()
                    .unwrap_or(fallback_agent.clone());
                agents.insert(forked_session_id.clone(), parent_agent);
            }

            if let Err(err) = ensure_session_loaded(state, &forked_session_id, "fork_session").await
            {
                let _ = send_message(
                    tx,
                    UiServerMessage::ForkResult {
                        success: false,
                        source_session_id: Some(source_session_id),
                        forked_session_id: Some(forked_session_id),
                        message: Some(format!("Fork created but failed to hydrate session: {err}")),
                    },
                )
                .await;
                return;
            }

            send_state(state, conn_id, tx).await;
            handle_list_sessions(state, tx).await;

            let _ = send_message(
                tx,
                UiServerMessage::ForkResult {
                    success: true,
                    source_session_id: Some(source_session_id),
                    forked_session_id: Some(forked_session_id),
                    message: None,
                },
            )
            .await;
        }
        Err(err) => {
            let _ = send_message(
                tx,
                UiServerMessage::ForkResult {
                    success: false,
                    source_session_id: Some(source_session_id),
                    forked_session_id: None,
                    message: Some(format!("Failed to fork session: {err}")),
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
    // Parse action string to enum
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

    if let Some(tx) =
        crate::elicitation::take_pending_elicitation_sender(state.agent.as_ref(), elicitation_id)
            .await
    {
        if tx.send(response).is_err() {
            log::warn!(
                "Elicitation response receiver dropped for elicitation_id={}",
                elicitation_id
            );
        }
    } else {
        log::warn!(
            "No pending elicitation found for elicitation_id={} (checked primary and delegates)",
            elicitation_id
        );
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
                let session_ref = {
                    let registry = state.agent.registry.lock().await;
                    registry.get(session_id).cloned()
                };
                if let Some(session_ref) = session_ref {
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
                let session_ref = {
                    let registry = state.agent.registry.lock().await;
                    registry.get(session_id).cloned()
                };
                if let Some(session_ref) = session_ref {
                    match session_ref.set_mode(new_mode).await {
                        Ok(_) => {
                            log::debug!(
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
        let session_ref = {
            let registry = state.agent.registry.lock().await;
            registry.get(&session_id).cloned()
        };

        if let Some(session_ref) = session_ref {
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

// ── Reasoning effort ──────────────────────────────────────────────────────────

/// Handle reasoning effort change request.
///
/// Sets the reasoning effort on the active session actor (per-session) and
/// updates the default for new sessions.
pub async fn handle_set_reasoning_effort(
    state: &ServerState,
    conn_id: &str,
    effort_str: &str,
    tx: &mpsc::Sender<String>,
) {
    let effort: Option<ReasoningEffort> = if effort_str.is_empty() || effort_str == "auto" {
        None
    } else {
        match serde_json::from_value::<ReasoningEffort>(serde_json::json!(effort_str)) {
            Ok(e) => Some(e),
            Err(e) => {
                let _ = send_error(
                    tx,
                    format!("Invalid reasoning effort '{}': {}", effort_str, e),
                )
                .await;
                return;
            }
        }
    };

    // Update default for new sessions (lock-free store via ArcSwap)
    state.agent.default_reasoning_effort.store(Arc::new(effort));

    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    if let Some(ref session_id) = session_id {
        let session_ref = {
            let registry = state.agent.registry.lock().await;
            registry.get(session_id).cloned()
        };
        // Always send to the actor — including None (auto) so the LLM config
        // row is updated and the next turn uses provider/model defaults.
        if let Some(session_ref) = session_ref {
            match session_ref.set_reasoning_effort(effort).await {
                Ok(_) => {
                    log::debug!(
                        "Reasoning effort changed on session {}: {:?}",
                        session_id,
                        effort
                    );
                }
                Err(e) => {
                    log::warn!(
                        "Failed to set reasoning effort on session {}: {}. Will apply to next session.",
                        session_id,
                        e
                    );
                }
            }
        }
    }

    let _ = send_message(
        tx,
        UiServerMessage::ReasoningEffort {
            reasoning_effort: effort.map(|e| e.to_string()),
        },
    )
    .await;
}

/// Handle get reasoning effort request.
pub async fn handle_get_reasoning_effort(
    state: &ServerState,
    conn_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    let effort = if let Some(session_id) = session_id {
        let session_ref = {
            let registry = state.agent.registry.lock().await;
            registry.get(&session_id).cloned()
        };

        if let Some(session_ref) = session_ref {
            session_ref.get_reasoning_effort().await.ok().flatten()
        } else {
            **state.agent.default_reasoning_effort.load()
        }
    } else {
        **state.agent.default_reasoning_effort.load()
    };

    let _ = send_message(
        tx,
        UiServerMessage::ReasoningEffort {
            reasoning_effort: effort.map(|e| e.to_string()),
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
