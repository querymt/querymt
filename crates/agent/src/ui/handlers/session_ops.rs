//! Session operation handlers.
//!
//! Covers the full lifecycle of a session visible from the UI:
//! listing, loading, cancelling, subscribing/unsubscribing, undo/redo,
//! elicitation responses, agent mode, file index, and LLM config queries.

use super::super::ServerState;
use super::super::connection::{send_error, send_message, send_state, subscribe_to_file_index};
use super::super::messages::{SessionGroup, SessionSummary, UiServerMessage};
#[cfg(feature = "remote")]
use super::remote::finalize_remote_session_attach;

use super::super::session::{
    PRIMARY_AGENT_ID, agent_for_profile_and_id, local_agent_for_session, mode_for_session,
    reasoning_effort_for_session, resolve_profile_id, resolve_profile_id_for_session,
    session_ref_for_session,
};
use crate::agent::LocalAgentHandle;
use crate::agent::core::AgentMode;
use crate::events::EventEnvelope;
use crate::index::resolve_workspace_root;
use crate::session::domain::ForkOrigin;
use crate::session::load_session_snapshot;
use crate::session::projection::SessionScope;
use crate::ui::cursor_from_events;
use crate::ui::mentions::{filter_index_for_cwd, filter_index_for_cwd_entries};
use agent_client_protocol::schema::{LoadSessionRequest, SessionId};
use querymt::chat::ReasoningEffort;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
#[cfg(feature = "remote")]
use tracing::Instrument;
#[cfg(feature = "remote")]
use tracing::info_span;

#[cfg(feature = "remote")]
pub(crate) fn refresh_attached_remote_summary(
    by_node: &mut std::collections::HashMap<String, Vec<SessionSummary>>,
    peer_label: &str,
    node_id: &str,
    session_info: &crate::agent::remote::RemoteSessionInfo,
) -> bool {
    let Some(existing) = by_node.get_mut(peer_label).and_then(|sessions| {
        sessions
            .iter_mut()
            .find(|s| s.session_id == session_info.session_id)
    }) else {
        return false;
    };

    if session_info.title.is_some() {
        existing.title = session_info.title.clone();
        existing.name = session_info.title.clone();
    }

    existing.node_id = Some(node_id.to_string());
    existing.runtime_state = session_info.runtime_state.clone();
    true
}

// ── Session list / load ───────────────────────────────────────────────────────

fn to_ui_summary(s: crate::session::projection::SessionListItem) -> SessionSummary {
    SessionSummary {
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
        fork_count: s.fork_count as u64,
        node: None,
        node_id: None,
        attached: None,
        runtime_state: None,
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListSessionsRequest {
    pub mode: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub cwd: Option<String>,
    pub query: Option<String>,
    pub session_scope: Option<SessionScope>,
    /// When true, merge remote mesh sessions into the response.
    /// Defaults to false so local session open is never blocked on remote discovery.
    pub include_remote: bool,
}

impl ListSessionsRequest {
    pub fn root_browse() -> Self {
        Self {
            session_scope: Some(SessionScope::Root),
            include_remote: false,
            ..Self::default()
        }
    }

    pub fn root_browse_with_remote() -> Self {
        Self {
            include_remote: true,
            ..Self::root_browse()
        }
    }
}

#[cfg(feature = "remote")]
async fn merge_remote_bookmarks_for_list(
    state: &ServerState,
    groups: &mut Vec<SessionGroup>,
) -> (usize, usize) {
    let mut remote_group_count = 0usize;
    let mut remote_session_count = 0usize;

    let node_id_by_label: std::collections::HashMap<String, String> =
        if state.agent.mesh().is_some() {
            state
                .get_remote_nodes_cached()
                .await
                .into_iter()
                .map(|n| (n.hostname, n.node_id.to_string()))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    let bookmark_titles: std::collections::HashMap<String, String> = state
        .session_store
        .list_remote_session_bookmarks()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|bookmark| bookmark.title.map(|title| (bookmark.session_id, title)))
        .collect();

    // Attached remote sessions are already in local state, so they should be
    // listed immediately without waiting on mesh discovery.
    let remote = {
        let registry = state.agent.registry.lock().await;
        registry.remote_sessions()
    };
    if !remote.is_empty() {
        let cwds = state.session_cwds.lock().await;
        for (session_id, peer_label, remote_node_id) in remote {
            let group_cwd = format!("remote::{}", peer_label);
            let summary = SessionSummary {
                session_id: session_id.clone(),
                name: bookmark_titles.get(&session_id).cloned(),
                cwd: cwds.get(&session_id).map(|p| p.display().to_string()),
                title: bookmark_titles.get(&session_id).cloned(),
                created_at: None,
                updated_at: None,
                parent_session_id: None,
                fork_origin: None,
                session_kind: None,
                has_children: false,
                fork_count: 0,
                node: Some(peer_label.clone()),
                node_id: remote_node_id.or_else(|| node_id_by_label.get(&peer_label).cloned()),
                attached: Some(true),
                runtime_state: None,
            };

            if let Some(existing) = groups
                .iter_mut()
                .find(|g| g.cwd.as_deref() == Some(group_cwd.as_str()))
            {
                if !existing
                    .sessions
                    .iter()
                    .any(|s| s.session_id == summary.session_id)
                {
                    remote_session_count += 1;
                    existing.sessions.push(summary);
                }
            } else {
                remote_group_count += 1;
                remote_session_count += 1;
                groups.push(SessionGroup {
                    cwd: Some(group_cwd),
                    sessions: vec![summary],
                    latest_activity: None,
                    total_count: None,
                    next_cursor: None,
                });
            }
        }
    }

    // Listing persisted bookmarks is a local, read-only operation; it should
    // not depend on mesh availability or trigger a remote attach.
    match state.session_store.list_remote_session_bookmarks().await {
        Ok(bookmarks) if !bookmarks.is_empty() => {
            let registry_ids: std::collections::HashSet<String> = {
                let registry = state.agent.registry.lock().await;
                registry.session_ids().into_iter().collect()
            };

            for bookmark in bookmarks {
                if registry_ids.contains(&bookmark.session_id) {
                    continue;
                }

                let group_cwd = format!("remote::{}", bookmark.peer_label);
                let summary = SessionSummary {
                    session_id: bookmark.session_id,
                    name: bookmark.title.clone(),
                    cwd: bookmark.cwd,
                    title: bookmark.title,
                    created_at: None,
                    updated_at: None,
                    parent_session_id: None,
                    fork_origin: None,
                    session_kind: None,
                    has_children: false,
                    fork_count: 0,
                    node: Some(bookmark.peer_label.clone()),
                    node_id: Some(bookmark.node_id),
                    attached: Some(false),
                    runtime_state: Some("stopped".to_string()),
                };

                if let Some(existing) = groups
                    .iter_mut()
                    .find(|g| g.cwd.as_deref() == Some(group_cwd.as_str()))
                {
                    if !existing
                        .sessions
                        .iter()
                        .any(|s| s.session_id == summary.session_id)
                    {
                        remote_session_count += 1;
                        existing.sessions.push(summary);
                    }
                } else {
                    remote_group_count += 1;
                    remote_session_count += 1;
                    groups.push(SessionGroup {
                        cwd: Some(group_cwd),
                        sessions: vec![summary],
                        latest_activity: None,
                        total_count: None,
                        next_cursor: None,
                    });
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("Failed to load remote session bookmarks: {}", e);
        }
    }

    (remote_group_count, remote_session_count)
}

#[cfg(feature = "remote")]
async fn merge_remote_sessions_for_list(
    state: ServerState,
    groups: &mut Vec<SessionGroup>,
) -> (usize, usize) {
    let mut remote_group_count = 0usize;
    let mut remote_session_count = 0usize;

    // 1. Collect already-attached remote sessions from the registry.
    let attached_sessions: std::collections::HashSet<String>;
    let remote = {
        let registry = state.agent.registry.lock().await;
        let sessions = registry.remote_sessions();
        attached_sessions = sessions.iter().map(|(id, _, _)| id.clone()).collect();
        sessions
    };

    let mut by_node: std::collections::HashMap<String, Vec<SessionSummary>> =
        std::collections::HashMap::new();
    let bookmark_titles: std::collections::HashMap<String, String> = state
        .session_store
        .list_remote_session_bookmarks()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|bookmark| bookmark.title.map(|title| (bookmark.session_id, title)))
        .collect();

    let node_id_by_label: std::collections::HashMap<String, String> =
        if state.agent.mesh().is_some() {
            state
                .get_remote_nodes_cached()
                .await
                .into_iter()
                .map(|n| (n.hostname, n.node_id.to_string()))
                .collect()
        } else {
            std::collections::HashMap::new()
        };

    if !remote.is_empty() {
        let cwds = state.session_cwds.lock().await;

        for (session_id, peer_label, remote_node_id) in remote {
            let cwd = cwds.get(&session_id).map(|p| p.display().to_string());
            let node_id = remote_node_id.or_else(|| node_id_by_label.get(&peer_label).cloned());
            let title = bookmark_titles.get(&session_id).cloned();
            by_node
                .entry(peer_label.clone())
                .or_default()
                .push(SessionSummary {
                    session_id,
                    name: title.clone(),
                    cwd: cwd.clone(),
                    title,
                    created_at: None,
                    updated_at: None,
                    parent_session_id: None,
                    fork_origin: None,
                    session_kind: None,
                    has_children: false,
                    fork_count: 0,
                    node: Some(peer_label),
                    node_id,
                    attached: Some(true),
                    runtime_state: None,
                });
        }
    }

    // 2. Query each live peer for their sessions and refresh metadata.
    let mut confirmed_peer_sessions: std::collections::HashMap<
        String,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();

    if state.agent.mesh().is_some() && !node_id_by_label.is_empty() {
        let peer_futures: Vec<_> = node_id_by_label
            .iter()
            .map(|(peer_label, node_id_str)| {
                let peer_label = peer_label.clone();
                let node_id_str = node_id_str.clone();
                let state = state.clone();
                async move {
                    let sessions =
                        match tokio::time::timeout(std::time::Duration::from_secs(2), async {
                            let nm_ref = state.agent.find_node_manager(&node_id_str).await?;
                            state.agent.list_remote_sessions(&nm_ref, None, None).await
                        })
                        .await
                        {
                            Ok(Ok(response)) => response.sessions,
                            Ok(Err(_)) | Err(_) => return None,
                        };
                    Some((peer_label, node_id_str, sessions))
                }
            })
            .collect();

        let peer_results = futures_util::future::join_all(peer_futures).await;

        for result in peer_results.into_iter().flatten() {
            let (peer_label, node_id_str, sessions) = result;

            let confirmed_ids: std::collections::HashSet<String> =
                sessions.iter().map(|s| s.session_id.clone()).collect();
            confirmed_peer_sessions.insert(peer_label.clone(), confirmed_ids);

            for session_info in sessions {
                if attached_sessions.contains(&session_info.session_id) {
                    let _ = refresh_attached_remote_summary(
                        &mut by_node,
                        &peer_label,
                        &node_id_str,
                        &session_info,
                    );
                    continue;
                }

                by_node
                    .entry(peer_label.clone())
                    .or_default()
                    .push(SessionSummary {
                        session_id: session_info.session_id,
                        name: session_info.title.clone(),
                        cwd: session_info.cwd,
                        title: session_info.title,
                        created_at: None,
                        updated_at: None,
                        parent_session_id: None,
                        fork_origin: None,
                        session_kind: None,
                        has_children: false,
                        fork_count: 0,
                        node: Some(peer_label.clone()),
                        node_id: Some(node_id_str.clone()),
                        attached: Some(false),
                        runtime_state: session_info.runtime_state,
                    });
            }
        }
    }

    for (node_label, sessions) in by_node {
        remote_group_count += 1;
        remote_session_count += sessions.len();
        groups.push(SessionGroup {
            cwd: Some(format!("remote::{}", node_label)),
            sessions,
            latest_activity: None,
            total_count: None,
            next_cursor: None,
        });
    }

    (remote_group_count, remote_session_count)
}

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
pub async fn handle_list_sessions(
    state: &ServerState,
    tx: &mpsc::Sender<String>,
    request: ListSessionsRequest,
) {
    let started = Instant::now();
    let ListSessionsRequest {
        mode,
        cursor,
        limit,
        cwd,
        query,
        session_scope,
        include_remote,
    } = request;
    let page_limit = limit.unwrap_or(20).clamp(1, 200) as usize;
    let mode = mode.unwrap_or_else(|| "browse".to_string());
    let session_scope = session_scope.unwrap_or_default();
    let is_browse_first_page = mode == "browse" && cursor.is_none();
    let should_merge_remote = include_remote
        && is_browse_first_page
        && matches!(session_scope, SessionScope::Root | SessionScope::All);

    let to_ui_group = |g: crate::session::projection::SessionGroup| SessionGroup {
        cwd: g.cwd,
        latest_activity: g.latest_activity.and_then(|t| t.format(&Rfc3339).ok()),
        total_count: Some(g.total_count.unwrap_or(g.sessions.len()) as u64),
        next_cursor: g.next_cursor,
        sessions: g.sessions.into_iter().map(to_ui_summary).collect(),
    };

    let result = match mode.as_str() {
        "group" => {
            let cwd_value = match cwd.as_deref() {
                Some("__none__") => None,
                _ => cwd,
            };
            state
                .view_store
                .list_group_sessions(cwd_value, cursor, page_limit, session_scope)
                .await
                .map(|(group, total)| {
                    let next_cursor = group.next_cursor.clone();
                    (vec![to_ui_group(group)], next_cursor, total)
                })
        }
        "search" => {
            let q = query.unwrap_or_default();
            state
                .view_store
                .search_sessions(q, cursor, page_limit, session_scope)
                .await
                .map(|(groups, next_cursor, total)| {
                    (
                        groups.into_iter().map(to_ui_group).collect(),
                        next_cursor,
                        total,
                    )
                })
        }
        _ => state
            .view_store
            .browse_session_groups(cursor, page_limit, 10, session_scope)
            .await
            .map(|(groups, next_cursor, total)| {
                (
                    groups.into_iter().map(to_ui_group).collect(),
                    next_cursor,
                    total,
                )
            }),
    };

    let (mut groups, next_cursor, total_count) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list sessions: {}", e)).await;
            return;
        }
    };

    #[cfg(feature = "remote")]
    let mut bookmark_group_count = 0usize;
    #[cfg(feature = "remote")]
    let mut bookmark_session_count = 0usize;
    #[cfg(feature = "remote")]
    if should_merge_remote {
        (bookmark_group_count, bookmark_session_count) =
            merge_remote_bookmarks_for_list(state, &mut groups).await;
    }

    let local_group_count = groups.len();
    let local_session_count: usize = groups.iter().map(|g| g.sessions.len()).sum();
    let total_group_count_after_bookmarks = groups.len();

    let span = tracing::Span::current();
    span.record("local_group_count", local_group_count);
    span.record("local_session_count", local_session_count);
    span.record("view_fetch_ms", started.elapsed().as_millis() as u64);

    let _ = send_message(
        tx,
        UiServerMessage::SessionList {
            groups: groups.clone(),
            next_cursor: next_cursor.clone(),
            total_count: total_count as u64,
        },
    )
    .await;

    #[cfg(feature = "remote")]
    if should_merge_remote && state.agent.mesh().is_some() {
        let state = state.clone();
        let tx = tx.clone();
        let next_cursor = next_cursor.clone();
        let mut merged_groups = groups;
        tokio::spawn(async move {
            let remote_merge_started = Instant::now();
            let (live_remote_group_count, live_remote_session_count) =
                merge_remote_sessions_for_list(state, &mut merged_groups)
                    .instrument(info_span!("ui.handle_list_sessions.remote_merge"))
                    .await;

            let total_group_count = merged_groups.len();
            let total_session_count: usize = merged_groups.iter().map(|g| g.sessions.len()).sum();

            let _ = send_message(
                &tx,
                UiServerMessage::SessionList {
                    groups: merged_groups,
                    next_cursor,
                    total_count: total_count as u64,
                },
            )
            .await;

            tracing::debug!(
                target: "querymt_agent::ui::handlers",
                operation = "ui.list_sessions.remote_merge",
                remote_group_count = bookmark_group_count + live_remote_group_count,
                remote_session_count = bookmark_session_count + live_remote_session_count,
                total_group_count,
                total_session_count,
                remote_merge_ms = remote_merge_started.elapsed().as_millis() as u64,
                total_ms = started.elapsed().as_millis() as u64,
                "remote session merge completed"
            );
        });
    }

    #[cfg(feature = "remote")]
    if should_merge_remote && state.agent.mesh().is_none() {
        span.record("remote_group_count", bookmark_group_count);
        span.record("remote_session_count", bookmark_session_count);
        span.record("total_group_count", total_group_count_after_bookmarks);
        span.record("total_session_count", local_session_count);
        span.record("remote_merge_ms", 0u64);
        span.record("total_ms", started.elapsed().as_millis() as u64);
    }

    #[cfg(not(feature = "remote"))]
    {
        span.record("remote_group_count", 0usize);
        span.record("remote_session_count", 0usize);
        span.record("total_group_count", total_group_count_after_bookmarks);
        span.record("total_session_count", local_session_count);
        span.record("remote_merge_ms", 0u64);
        span.record("total_ms", started.elapsed().as_millis() as u64);
    }
}

/// Handle session loading request.
pub async fn handle_list_session_children(
    state: &ServerState,
    tx: &mpsc::Sender<String>,
    parent_session_id: String,
    cursor: Option<String>,
    limit: Option<u32>,
    session_scope: Option<SessionScope>,
) {
    match session_scope {
        None | Some(SessionScope::Forks) => {}
        Some(_) => {
            let _ = send_error(
                tx,
                "Session children list only supports user forks".to_string(),
            )
            .await;
            return;
        }
    }

    let page_limit = limit.unwrap_or(20).clamp(1, 200) as usize;
    let result = state
        .view_store
        .list_session_children(parent_session_id.clone(), cursor, page_limit)
        .await;

    let (group, total_count) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list session children: {}", e)).await;
            return;
        }
    };

    let _ = send_message(
        tx,
        UiServerMessage::SessionChildren {
            parent_session_id,
            sessions: group.sessions.into_iter().map(to_ui_summary).collect(),
            next_cursor: group.next_cursor,
            total_count: total_count as u64,
        },
    )
    .await;
}

pub async fn handle_load_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let snapshot =
        match load_session_snapshot(&state.agent, state.view_store.clone(), session_id).await {
            Ok(snapshot) => snapshot,
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
        // Remote sessions may have cwd cached from attach/list metadata.
        let cwds = state.session_cwds.lock().await;
        cwds.get(session_id).cloned()
    };

    // 2. Determine profile/agent IDs from the memory/cache binding, or fall back active.
    let profile_id = if let Some(profiles) = &state.profiles {
        if let Some(binding) = profiles.session_binding(session_id).await {
            Some(binding.profile_id)
        } else {
            resolve_profile_id(state, None).await.unwrap_or_default()
        }
    } else {
        None
    };
    let agent_id = PRIMARY_AGENT_ID.to_string();

    if let Err(e) =
        ensure_session_loaded(state, session_id, profile_id.as_deref(), "load_session").await
    {
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
    let cursor = snapshot.cursor.clone();

    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            agent_id,
            profile_id,
            audit: snapshot.audit,
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

async fn remove_session_actor_from_agent(agent: &Arc<LocalAgentHandle>, session_id: &str) {
    let mut registry = agent.registry.lock().await;
    // For remote sessions, send UnsubscribeEvents before removing so the remote
    // EventForwarder task is properly cleaned up.
    #[cfg(feature = "remote")]
    {
        registry.detach_remote_session(session_id).await;
    }
    #[cfg(not(feature = "remote"))]
    {
        registry.remove(session_id);
    }
}

/// Handle session deletion request.
pub async fn handle_delete_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    let bound_agent = local_agent_for_session(state, Some(session_id), None)
        .await
        .ok();

    if let Err(err) = state.session_store.delete_session(session_id).await {
        let _ = send_error(tx, format!("Failed to delete session: {}", err)).await;
        return;
    }

    if let Some(agent) = bound_agent.as_ref() {
        remove_session_actor_from_agent(agent, session_id).await;
    }
    let removed_from_root = bound_agent
        .as_ref()
        .is_some_and(|agent| Arc::ptr_eq(agent, &state.agent));
    if !removed_from_root {
        remove_session_actor_from_agent(&state.agent, session_id).await;
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

    if let Some(profiles) = &state.profiles {
        profiles.forget_session_binding(session_id).await;
    }

    send_state(state, conn_id, tx).await;
    handle_list_sessions(state, tx, ListSessionsRequest::root_browse()).await;
}

pub(super) async fn ensure_session_loaded(
    state: &ServerState,
    session_id: &str,
    profile_id: Option<&str>,
    op_name: &'static str,
) -> Result<(), String> {
    if state
        .agent
        .registry
        .lock()
        .await
        .get(session_id)
        .is_some_and(|session_ref| session_ref.is_remote())
    {
        return Ok(());
    }

    let selected_profile_id = if let Some(profile_id) = profile_id {
        Some(profile_id.to_string())
    } else if let Some(profiles) = &state.profiles {
        if let Some(binding) = profiles.session_binding(session_id).await {
            Some(binding.profile_id)
        } else {
            // Only explicit/persisted bindings are sticky: fallback loads use the active profile
            // without binding provenance unless first-open claiming becomes policy later.
            resolve_profile_id(state, None).await?
        }
    } else {
        None
    };

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

    let agent =
        agent_for_profile_and_id(state, selected_profile_id.as_deref(), PRIMARY_AGENT_ID).await?;
    let req = LoadSessionRequest::new(
        SessionId::from(session_id.to_string()),
        session.cwd.unwrap_or_else(PathBuf::new),
    );
    agent.load_session(req).await.map_err(|e| e.to_string())?;

    tracing::info!(
        op_name,
        session_id,
        store_exists = true,
        actor_loaded = true,
        "session lazy hydration evaluated"
    );

    Ok(())
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

    let session_ref = match session_ref_for_session(state, &session_id).await {
        Some(session_ref) => session_ref,
        None => {
            let _ = send_error(
                tx,
                format!("No active session runtime for '{}'", session_id),
            )
            .await;
            return;
        }
    };

    if let Err(e) = session_ref.cancel().await {
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

    let profile_id = match &state.profiles {
        Some(profiles) => profiles
            .session_binding(session_id)
            .await
            .map(|binding| binding.profile_id),
        None => None,
    };

    let _ = send_message(
        tx,
        UiServerMessage::SessionEvents {
            session_id: session_id.to_string(),
            agent_id: resolved_agent_id,
            profile_id,
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

    let session_ref = match session_ref_for_session(state, &session_id).await {
        Some(session_ref) => session_ref,
        None => {
            let undo_stack = load_undo_stack(state, &session_id).await;
            let _ = send_message(
                tx,
                UiServerMessage::UndoResult {
                    success: false,
                    message: Some(format!("Session not found: {}", session_id)),
                    reverted_files: vec![],
                    message_id: None,
                    undo_stack,
                },
            )
            .await;
            return;
        }
    };

    match session_ref.undo(message_id.to_string()).await {
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

    let session_ref = match session_ref_for_session(state, &session_id).await {
        Some(session_ref) => session_ref,
        None => {
            let undo_stack = load_undo_stack(state, &session_id).await;
            let _ = send_message(
                tx,
                UiServerMessage::RedoResult {
                    success: false,
                    message: Some(format!("Session not found: {}", session_id)),
                    undo_stack,
                },
            )
            .await;
            return;
        }
    };

    match session_ref.redo().await {
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

    let source_profile_id = resolve_profile_id_for_session(state, Some(&source_session_id), None)
        .await
        .unwrap_or_default();
    #[cfg(feature = "remote")]
    let source_agent = match local_agent_for_session(state, Some(&source_session_id), None).await {
        Ok(agent) => agent,
        Err(err) => {
            let _ = send_message(
                tx,
                UiServerMessage::ForkResult {
                    success: false,
                    source_session_id: Some(source_session_id),
                    forked_session_id: None,
                    message: Some(format!("Failed to resolve source session runtime: {err}")),
                },
            )
            .await;
            return;
        }
    };
    let source_session_ref = session_ref_for_session(state, &source_session_id).await;

    #[cfg(feature = "remote")]
    if let Some(crate::agent::remote::SessionActorRef::Remote { .. }) = source_session_ref.as_ref()
    {
        let source_remote_node_id = state
            .session_store
            .list_remote_session_bookmarks()
            .await
            .ok()
            .and_then(|bookmarks| {
                bookmarks
                    .into_iter()
                    .find(|bookmark| bookmark.session_id == source_session_id)
                    .map(|bookmark| bookmark.node_id)
            });

        let Some(node_id) = source_remote_node_id else {
            let _ = send_message(
                tx,
                UiServerMessage::ForkResult {
                    success: false,
                    source_session_id: Some(source_session_id),
                    forked_session_id: None,
                    message: Some(
                        "Remote source session is missing owner node metadata".to_string(),
                    ),
                },
            )
            .await;
            return;
        };

        let node_manager_ref = match source_agent.find_node_manager(&node_id).await {
            Ok(r) => r,
            Err(err) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::ForkResult {
                        success: false,
                        source_session_id: Some(source_session_id),
                        forked_session_id: None,
                        message: Some(format!(
                            "Failed to resolve remote node manager: {}",
                            err.message
                        )),
                    },
                )
                .await;
                return;
            }
        };

        match source_agent
            .fork_remote_session(
                &node_manager_ref,
                source_session_id.clone(),
                message_id.to_string(),
            )
            .await
        {
            Ok(resp) => {
                let forked_session_id = resp.session_id.clone();
                let cwd = resp.cwd.as_ref().map(PathBuf::from);
                if let (Some(profiles), Some(profile_id)) =
                    (&state.profiles, source_profile_id.as_deref())
                    && let Err(err) = profiles
                        .bind_session_to_profile(forked_session_id.clone(), profile_id)
                        .await
                {
                    let _ = send_message(
                        tx,
                        UiServerMessage::ForkResult {
                            success: false,
                            source_session_id: Some(source_session_id),
                            forked_session_id: Some(forked_session_id),
                            message: Some(format!(
                                "Fork created but failed to bind profile: {err}"
                            )),
                        },
                    )
                    .await;
                    return;
                }

                if let Err(err) = finalize_remote_session_attach(
                    state,
                    conn_id,
                    &node_id,
                    &forked_session_id,
                    resp.handoff,
                    cwd,
                    tx,
                )
                .await
                {
                    let _ = send_message(
                        tx,
                        UiServerMessage::ForkResult {
                            success: false,
                            source_session_id: Some(source_session_id),
                            forked_session_id: Some(forked_session_id),
                            message: Some(format!(
                                "Fork created but failed to attach remote child: {err}"
                            )),
                        },
                    )
                    .await;
                    return;
                }

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
                return;
            }
            Err(err) => {
                let _ = send_message(
                    tx,
                    UiServerMessage::ForkResult {
                        success: false,
                        source_session_id: Some(source_session_id),
                        forked_session_id: None,
                        message: Some(format!("Failed to fork remote session: {}", err.message)),
                    },
                )
                .await;
                return;
            }
        }
    }

    let fork_result = if let Some(session_ref) = source_session_ref {
        session_ref
            .fork_at_message(message_id.to_string())
            .await
            .map_err(|err| err.to_string())
    } else {
        state
            .session_store
            .fork_session(&source_session_id, message_id, ForkOrigin::User)
            .await
            .map_err(|err| err.to_string())
    };

    match fork_result {
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

            if let (Some(profiles), Some(profile_id)) =
                (&state.profiles, source_profile_id.as_deref())
                && let Err(err) = profiles
                    .bind_session_to_profile(forked_session_id.clone(), profile_id)
                    .await
            {
                let _ = send_message(
                    tx,
                    UiServerMessage::ForkResult {
                        success: false,
                        source_session_id: Some(source_session_id),
                        forked_session_id: Some(forked_session_id),
                        message: Some(format!("Fork created but failed to bind profile: {err}")),
                    },
                )
                .await;
                return;
            }

            if let Err(err) = ensure_session_loaded(
                state,
                &forked_session_id,
                source_profile_id.as_deref(),
                "fork_session",
            )
            .await
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
            handle_list_sessions(state, tx, ListSessionsRequest::root_browse()).await;

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

            let previous_mode = mode_for_session(state, session_id.as_deref()).await;

            match local_agent_for_session(state, session_id.as_deref(), None).await {
                Ok(agent) => {
                    if let Ok(mut default_mode) = agent.default_mode.lock() {
                        *default_mode = new_mode;
                    }
                }
                Err(e) => {
                    let _ =
                        send_error(tx, format!("Failed to resolve session runtime: {}", e)).await;
                    return;
                }
            }

            let mode_set_on_actor = if let Some(ref session_id) = session_id {
                if let Some(session_ref) = session_ref_for_session(state, session_id).await {
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

    let mode = mode_for_session(state, session_id.as_deref()).await;

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

    let session_id = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(&conn.active_agent_id).cloned())
    };

    match local_agent_for_session(state, session_id.as_deref(), None).await {
        Ok(agent) => {
            // Update default for new sessions in the session's profile runtime.
            agent.default_reasoning_effort.store(Arc::new(effort));
        }
        Err(e) => {
            let _ = send_error(tx, format!("Failed to resolve session runtime: {}", e)).await;
            return;
        }
    }

    if let Some(ref session_id) = session_id {
        // Always send to the actor — including None (auto) so the LLM config
        // row is updated and the next turn uses provider/model defaults.
        if let Some(session_ref) = session_ref_for_session(state, session_id).await {
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

    let effort = reasoning_effort_for_session(state, session_id.as_deref()).await;

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
    if let Some(actor_ref) = session_ref_for_session(state, &session_id).await
        && actor_ref.is_remote()
    {
        let cwd = {
            let cwds = state.session_cwds.lock().await;
            cwds.get(&session_id).cloned()
        };

        match actor_ref.get_file_index().await {
            Ok(resp) => {
                let root = std::path::PathBuf::from(&resp.workspace_root);
                let files = match cwd.as_ref().and_then(|c| c.strip_prefix(&root).ok()) {
                    Some(relative_cwd) => filter_index_for_cwd_entries(&resp.files, relative_cwd),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_browse_with_remote_includes_remote_sessions() {
        let request = ListSessionsRequest::root_browse_with_remote();

        assert_eq!(request.session_scope, Some(SessionScope::Root));
        assert!(request.include_remote);
    }
}
