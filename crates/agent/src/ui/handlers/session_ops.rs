//! Session operation handlers.
//!
//! Covers the full lifecycle of a session visible from the UI:
//! listing, loading, cancelling, subscribing/unsubscribing, undo/redo,
//! elicitation responses, agent mode, file index, and LLM config queries.

use super::super::ServerState;
use super::super::connection::{
    send_error, send_message, send_session_error, send_state, subscribe_to_file_index,
};
#[cfg(all(test, feature = "remote"))]
use super::super::messages::SessionSummary;
use super::super::messages::UiServerMessage;
#[cfg(feature = "remote")]
use super::remote::finalize_remote_session_attach;

use super::super::session::{
    PRIMARY_AGENT_ID, agent_for_profile_and_id, local_agent_for_session, mode_for_session,
    reasoning_effort_for_session, resolve_profile_id, resolve_profile_id_for_session,
    session_ref_for_session,
};
use crate::acp::protocol::{LoadSessionRequest, SessionId};
use crate::agent::LocalAgentHandle;
use crate::agent::core::AgentMode;
#[cfg(feature = "remote")]
use crate::agent::handle::remote_connect::RemoteConnectReason;
use crate::api::{
    AgentSessions, ListSessionsOptions, RemoteSessionConnectionState, RemoteSessionMode,
    SessionListMode,
};
use crate::events::EventEnvelope;
use crate::index::{get_or_create_workspace_with_timeout, resolve_workspace_root};
use crate::session::SessionLoadSnapshot;
use crate::session::domain::ForkOrigin;
use crate::session::load_session_snapshot;
use crate::session::projection::SessionScope;
use crate::session::store::RemoteSessionBookmark;
use crate::ui::cursor_from_events;
use crate::ui::mentions::{filter_index_for_cwd, filter_index_for_cwd_entries};
use querymt::chat::ReasoningEffort;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

#[cfg(all(test, feature = "remote"))]
pub(crate) fn refresh_attached_remote_summary(
    by_node: &mut std::collections::HashMap<String, Vec<SessionSummary>>,
    peer_label: &str,
    node_id: &str,
    session_info: &crate::agent::remote::RemoteSessionSnapshot,
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
    existing.attached = Some(true);
    existing.connection_state = Some(RemoteSessionConnectionState::Connected);
    existing.runtime_state = session_info.runtime_state.clone();
    true
}

// ── Session list / load ───────────────────────────────────────────────────────

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
        first_send_ms = tracing::field::Empty,
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
    let mode_name = mode.unwrap_or_else(|| "browse".to_string());
    let session_scope = session_scope.unwrap_or_default();
    let is_browse_first_page = mode_name == "browse" && cursor.is_none();
    let should_merge_remote = include_remote
        && is_browse_first_page
        && matches!(session_scope, SessionScope::Root | SessionScope::All);

    let mode = match mode_name.as_str() {
        "group" => SessionListMode::Group,
        "search" => SessionListMode::Search,
        _ => SessionListMode::Browse,
    };
    let remote = if should_merge_remote {
        RemoteSessionMode::Bookmarks
    } else {
        RemoteSessionMode::None
    };

    let page = match AgentSessions::new(
        state.agent.clone(),
        state.view_store.clone(),
        state.session_store.clone(),
        state.default_cwd.clone(),
    )
    .list(ListSessionsOptions {
        mode,
        cursor,
        limit,
        cwd,
        query,
        session_scope: Some(session_scope),
        remote,
    })
    .await
    {
        Ok(page) => page,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list sessions: {}", e)).await;
            return;
        }
    };

    let groups = page.groups;
    let next_cursor = page.next_cursor;
    let total_count = page.total_count;
    let local_group_count = groups.len();
    let local_session_count: usize = groups.iter().map(|g| g.sessions.len()).sum();

    let span = tracing::Span::current();
    span.record("local_group_count", local_group_count);
    span.record("local_session_count", local_session_count);
    span.record("view_fetch_ms", started.elapsed().as_millis() as u64);

    let first_send_started = Instant::now();
    let _ = send_message(
        tx,
        UiServerMessage::SessionList {
            groups: groups.clone(),
            next_cursor: next_cursor.clone(),
            total_count,
        },
    )
    .await;
    let first_send_ms = first_send_started.elapsed().as_millis() as u64;
    span.record("first_send_ms", first_send_ms);
    tracing::debug!(
        target: "querymt_agent::ui::handlers",
        operation = "ui.list_sessions.first_send",
        local_group_count,
        local_session_count,
        remote_group_count = 0usize,
        remote_session_count = 0usize,
        total_group_count = local_group_count,
        total_session_count = local_session_count,
        view_fetch_ms = started.elapsed().as_millis() as u64,
        first_send_ms,
        total_ms = started.elapsed().as_millis() as u64,
        "local session list sent"
    );

    #[cfg(feature = "remote")]
    if should_merge_remote && state.agent.mesh().is_some() {
        let state = state.clone();
        let tx = tx.clone();
        let next_cursor = next_cursor.clone();
        tokio::spawn(async move {
            let remote_merge_started = Instant::now();
            let page = match AgentSessions::new(
                state.agent.clone(),
                state.view_store.clone(),
                state.session_store.clone(),
                state.default_cwd.clone(),
            )
            .list(ListSessionsOptions {
                mode,
                cursor: None,
                limit: Some(20),
                cwd: None,
                query: None,
                session_scope: Some(session_scope),
                remote: RemoteSessionMode::Live,
            })
            .await
            {
                Ok(page) => page,
                Err(err) => {
                    tracing::debug!(target: "querymt_agent::ui::handlers", error = %err, "remote session merge failed");
                    return;
                }
            };

            let total_group_count = page.groups.len();
            let total_session_count: usize = page.groups.iter().map(|g| g.sessions.len()).sum();

            let _ = send_message(
                &tx,
                UiServerMessage::SessionList {
                    groups: page.groups,
                    next_cursor,
                    total_count,
                },
            )
            .await;

            tracing::debug!(
                target: "querymt_agent::ui::handlers",
                operation = "ui.list_sessions.remote_merge",
                local_group_count,
                local_session_count,
                remote_group_count = total_group_count.saturating_sub(local_group_count),
                remote_session_count = total_session_count.saturating_sub(local_session_count),
                total_group_count,
                total_session_count,
                first_send_ms,
                remote_merge_ms = remote_merge_started.elapsed().as_millis() as u64,
                total_ms = started.elapsed().as_millis() as u64,
                "remote session merge completed"
            );
        });
    }

    #[cfg(feature = "remote")]
    if should_merge_remote && state.agent.mesh().is_none() {
        span.record("remote_group_count", 0usize);
        span.record("remote_session_count", 0usize);
        span.record("total_group_count", local_group_count);
        span.record("total_session_count", local_session_count);
        span.record("remote_merge_ms", 0u64);
        span.record("total_ms", started.elapsed().as_millis() as u64);
    }

    #[cfg(not(feature = "remote"))]
    {
        span.record("remote_group_count", 0usize);
        span.record("remote_session_count", 0usize);
        span.record("total_group_count", local_group_count);
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

    let page = match AgentSessions::new(
        state.agent.clone(),
        state.view_store.clone(),
        state.session_store.clone(),
        state.default_cwd.clone(),
    )
    .children(parent_session_id.clone(), cursor, limit)
    .await
    {
        Ok(page) => page,
        Err(e) => {
            let _ = send_error(tx, format!("Failed to list session children: {}", e)).await;
            return;
        }
    };

    let _ = send_message(
        tx,
        UiServerMessage::SessionChildren {
            parent_session_id,
            sessions: page.sessions,
            next_cursor: page.next_cursor,
            total_count: page.total_count,
        },
    )
    .await;
}

/// Refuse to let an operation-hydration path treat a bookmarked-but-unattached
/// remote session as a local one.
///
/// A persisted remote bookmark is authoritative durable identity independent of
/// the live registry. When a session ID has a bookmark but no live remote
/// attachment, local hydration/creation must not run, and the ID must never be
/// reported as plain "not found". Returns the explicit
/// `remote_session_unavailable` signal for a later reconnect phase.
///
/// The session-open path (`handle_load_session`) does not use this guard: it
/// resolves durable location itself and serves offline-first snapshots (plan
/// §11). Operations that require hydration still refuse here.
async fn guard_bookmarked_remote_unattached(
    state: &ServerState,
    session_id: &str,
    op_name: &'static str,
) -> Result<(), String> {
    let bookmarked = state
        .agent
        .config
        .provider
        .history_store()
        .get_remote_session_bookmark(session_id)
        .await
        .map_err(|e| {
            format!("{op_name}: remote identity lookup failed for session '{session_id}': {e}")
        })?
        .is_some();
    if !bookmarked {
        return Ok(());
    }

    // Only stop when there is no live remote attachment we can simply reuse.
    let live_remote = state
        .agent
        .registry
        .lock()
        .await
        .get(session_id)
        .is_some_and(|session_ref| session_ref.is_remote());
    if live_remote {
        return Ok(());
    }

    Err(format!(
        "{op_name}: remote_session_unavailable: session '{session_id}' is bookmarked on a remote node but not currently connected; reconnect to open it"
    ))
}

/// Durable-location routing for the unified backend open-session behavior
/// (plan §11/§13). Resolution follows durable identity only: a registered
/// remote attachment may enrich the result but never determines location
/// (plan invariant 1).
enum OpenSessionRoute {
    /// A local session row exists and no remote bookmark conflicts with it.
    Local,
    /// A remote bookmark is the durable identity. `None` keeps a legacy
    /// attached session that predates bookmark persistence openable through
    /// the attached-remote flow (mixed-version compatibility).
    Remote(Option<RemoteSessionBookmark>),
    /// A local row and a remote bookmark both exist — typed conflict.
    Conflict,
    /// Neither a local row, a bookmark, nor a live attachment exists.
    NotFound,
}

async fn resolve_open_session_route(
    state: &ServerState,
    session_id: &str,
) -> Result<OpenSessionRoute, String> {
    let store = state.agent.config.provider.history_store();
    let (bookmark, local_row) = tokio::join!(
        store.get_remote_session_bookmark(session_id),
        store.get_session(session_id),
    );
    let bookmark = bookmark
        .map_err(|e| format!("remote identity lookup failed for session '{session_id}': {e}"))?;
    let local_row = local_row
        .map_err(|e| format!("local session lookup failed for session '{session_id}': {e}"))?;

    Ok(match (local_row.is_some(), bookmark) {
        (true, Some(_)) => OpenSessionRoute::Conflict,
        (true, None) => OpenSessionRoute::Local,
        (false, Some(bookmark)) => OpenSessionRoute::Remote(Some(bookmark)),
        (false, None) => {
            // Neither durable record. A live remote attachment without a
            // bookmark is a pre-upgrade attachment: keep it openable through
            // the attached-remote flow instead of reporting it as not found.
            let live_remote = state
                .agent
                .registry
                .lock()
                .await
                .get(session_id)
                .is_some_and(|session_ref| session_ref.is_remote());
            if live_remote {
                OpenSessionRoute::Remote(None)
            } else {
                OpenSessionRoute::NotFound
            }
        }
    })
}

pub async fn handle_load_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    tx: &mpsc::Sender<String>,
) {
    // Unified open behavior (plan §11): resolve authoritative durable location
    // before any hydration. A bookmarked remote ID must never enter local
    // hydration/creation, an unknown ID must stay distinct from an unavailable
    // remote node, and a durable local/remote collision is a typed conflict.
    match resolve_open_session_route(state, session_id).await {
        Err(lookup_error) => {
            let _ = send_session_error(
                tx,
                format!(
                    "Failed to load session: load_session: session_lookup_failed: {lookup_error}"
                ),
                None,
                Some(session_id),
            )
            .await;
        }
        Ok(OpenSessionRoute::Conflict) => {
            tracing::warn!(
                session_id,
                "session_location_conflict: session exists both as a local session and a remote bookmark"
            );
            let _ = send_session_error(
                tx,
                format!(
                    "Failed to load session: load_session: session_location_conflict: session '{session_id}' exists both as a local session and a remote bookmark"
                ),
                Some("session_location_conflict"),
                Some(session_id),
            )
            .await;
        }
        Ok(OpenSessionRoute::NotFound) => {
            let _ = send_session_error(
                tx,
                format!(
                    "Failed to load session: load_session: session_not_found: no local session or remote bookmark for '{session_id}'"
                ),
                Some("session_not_found"),
                Some(session_id),
            )
            .await;
        }
        Ok(OpenSessionRoute::Remote(bookmark)) => {
            open_remote_bookmarked_session(state, conn_id, session_id, bookmark, tx).await;
        }
        Ok(OpenSessionRoute::Local) => {
            open_local_session(state, conn_id, session_id, tx).await;
        }
    }
}

/// Local-session open path (plan §11.2): existing hydration behavior retained.
async fn open_local_session(
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

    // Load session to get cwd and populate session_cwds
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

    // Determine profile/agent IDs from the memory/cache binding, or fall back active.
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

    finish_session_open(
        state, conn_id, session_id, agent_id, profile_id, cwd_path, snapshot, None, None, tx,
    )
    .await;
}

/// Offline-first open of a remote-bookmarked session (plan §11).
///
/// Cached journal history is served before connectivity: the snapshot is
/// loaded first, then a bounded foreground recovery attempt runs
/// (`RemoteConnectReason::Open` budget, 5s by default — within the plan's
/// 3-5s guidance). Recovery failure is non-fatal: the cached snapshot is
/// returned with bookmark metadata and the session stays disconnected. Local
/// hydration/creation is never attempted for this ID, so
/// `ensure_session_loaded` is skipped entirely (it is a no-op for attached
/// remote sessions and would wrongly report "Session not found" for a
/// bookmark-only one).
#[tracing::instrument(
    name = "remote.session.resolve",
    skip_all,
    fields(session_id = %session_id)
)]
async fn open_remote_bookmarked_session(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    bookmark: Option<RemoteSessionBookmark>,
    tx: &mpsc::Sender<String>,
) {
    // Seed cwd from bookmark metadata (plan §11.4) so the workspace context
    // survives offline opens. An existing entry wins.
    if let Some(cwd) = bookmark.as_ref().and_then(|b| b.cwd.clone()) {
        let mut cwds = state.session_cwds.lock().await;
        cwds.entry(session_id.to_string())
            .or_insert(PathBuf::from(cwd));
    }

    // 1. Cached snapshot first — connectivity is never required to display
    //    cached remote history (plan invariant 9).
    let mut snapshot =
        match load_session_snapshot(&state.agent, state.view_store.clone(), session_id).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                let _ = send_error(tx, format!("Failed to load session: {}", e)).await;
                return;
            }
        };

    // 2. Bounded foreground recovery while detached. Failure is non-fatal:
    //    the cached snapshot is served as disconnected either way.
    #[cfg(feature = "remote")]
    let (connected, recovered) = {
        let attached = state
            .agent
            .registry
            .lock()
            .await
            .get(session_id)
            .is_some_and(|session_ref| session_ref.is_remote());
        if attached {
            (true, false)
        } else {
            match state
                .agent
                .ensure_remote_session_connected(
                    session_id,
                    bookmark.as_ref().map(|b| b.node_id.as_str()),
                    RemoteConnectReason::Open,
                )
                .await
            {
                Ok(connected) => {
                    tracing::debug!(
                        session_id,
                        outcome = connected.outcome.as_str(),
                        attachment_id = connected.attachment_id,
                        "remote session recovered during open"
                    );
                    (true, true)
                }
                Err(e) => {
                    tracing::info!(
                        session_id,
                        node_id = bookmark.as_ref().map(|b| b.node_id.as_str()).unwrap_or(""),
                        code = e.code(),
                        "remote open-time recovery failed; serving cached snapshot as disconnected"
                    );
                    (false, false)
                }
            }
        }
    };
    #[cfg(not(feature = "remote"))]
    let (connected, recovered): (bool, bool) = (false, false);

    // 3. On success, reload the snapshot so events relayed while the
    //    attachment was established are included. Exact cursor-based backfill
    //    of missed events is Phase 10.
    if recovered {
        if let Ok(fresh) =
            load_session_snapshot(&state.agent, state.view_store.clone(), session_id).await
        {
            snapshot = fresh;
        }
    }

    // Profile metadata follows the same rules as the local open: a persisted
    // binding wins, otherwise the active profile.
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

    let cwd_path = state.session_cwds.lock().await.get(session_id).cloned();

    let connection_state = if connected {
        Some(RemoteSessionConnectionState::Connected)
    } else {
        Some(RemoteSessionConnectionState::Disconnected)
    };

    finish_session_open(
        state,
        conn_id,
        session_id,
        agent_id,
        profile_id,
        cwd_path,
        snapshot,
        connection_state,
        bookmark.map(|b| b.node_id),
        tx,
    )
    .await;
}

/// Shared tail of the unified open flow: register the connection mapping,
/// deliver the hydrated snapshot, seed the replay cursor, and refresh state.
async fn finish_session_open(
    state: &ServerState,
    conn_id: &str,
    session_id: &str,
    agent_id: String,
    profile_id: Option<String>,
    cwd_path: Option<PathBuf>,
    snapshot: SessionLoadSnapshot,
    connection_state: Option<RemoteSessionConnectionState>,
    bookmark_node_id: Option<String>,
    tx: &mpsc::Sender<String>,
) {
    #[cfg(not(feature = "remote"))]
    let _ = bookmark_node_id;

    // Register in connection state
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.clone(), session_id.to_string());
            conn.subscribed_sessions.insert(session_id.to_string());
        }
    }

    // Register agent mapping
    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.to_string(), agent_id.clone());
    }

    // Send loaded audit view and persisted undo stack for UI hydration
    let undo_stack = load_undo_stack(state, session_id).await;
    let cursor = snapshot.cursor.clone();

    // Node identity: the live attachment wins; a disconnected open falls back
    // to the durable bookmark (plan §11.4).
    #[cfg(feature = "remote")]
    let loaded_session_node_id = {
        let registry = state.agent.registry.lock().await;
        registry
            .remote_sessions()
            .into_iter()
            .find_map(|(remote_session_id, _, remote_node_id)| {
                (remote_session_id == session_id).then_some(remote_node_id)
            })
            .flatten()
            .or(bookmark_node_id)
    };

    let _ = send_message(
        tx,
        UiServerMessage::SessionLoaded {
            session_id: session_id.to_string(),
            agent_id,
            profile_id,
            #[cfg(feature = "remote")]
            node_id: loaded_session_node_id,
            #[cfg(not(feature = "remote"))]
            node_id: None,
            connection_state,
            audit: snapshot.audit,
            undo_stack,
            cursor: cursor.clone(),
        },
    )
    .await;

    // Seed per-connection stream cursor from replay tail
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.session_cursors
                .insert(session_id.to_string(), cursor.clone());
        }
    }

    // Send updated state
    send_state(state, conn_id, tx).await;

    // Subscribe to file index updates if this session has a cwd
    if let Some(cwd) = cwd_path {
        let root = resolve_workspace_root(&cwd);
        subscribe_to_file_index(state.clone(), conn_id.to_string(), tx.clone(), root).await;
    }
}

async fn remove_session_actor_from_agent(agent: &Arc<LocalAgentHandle>, session_id: &str) {
    #[cfg(feature = "remote")]
    {
        if agent
            .detach_remote_session_attachment(session_id, true)
            .await
            .is_none()
        {
            agent.registry.lock().await.remove(session_id);
        }
    }
    #[cfg(not(feature = "remote"))]
    {
        agent.registry.lock().await.remove(session_id);
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
    if let Err(err) = state
        .session_store
        .remove_remote_session_bookmark(session_id)
        .await
    {
        let _ = send_error(tx, format!("Failed to delete remote bookmark: {}", err)).await;
        return;
    }

    if let Some(agent) = bound_agent.as_ref() {
        agent.clear_delegate_model_overrides(session_id).await;
        remove_session_actor_from_agent(agent, session_id).await;
    }
    let removed_from_root = bound_agent
        .as_ref()
        .is_some_and(|agent| Arc::ptr_eq(agent, &state.agent));
    if !removed_from_root {
        state.agent.clear_delegate_model_overrides(session_id).await;
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

    guard_bookmarked_remote_unattached(state, session_id, op_name).await?;

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

    #[cfg(feature = "remote")]
    let result = state
        .agent
        .execute_session_operation(
            &session_id,
            crate::agent::handle::session_operation::SessionOperation::Cancel,
            |session_ref| Box::pin(session_ref.cancel()),
        )
        .await
        .map_err(|error| error.into_agent_error());
    #[cfg(not(feature = "remote"))]
    let result = match session_ref_for_session(state, &session_id).await {
        Some(session_ref) => session_ref.cancel().await,
        None => Err(crate::error::AgentError::SessionNotFound {
            session_id: session_id.clone(),
        }),
    };

    if let Err(e) = result {
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

    #[cfg(feature = "remote")]
    let message_id = message_id.to_string();
    #[cfg(feature = "remote")]
    let result = state
        .agent
        .execute_session_operation(
            &session_id,
            crate::agent::handle::session_operation::SessionOperation::Undo,
            |session_ref| {
                let message_id = message_id.clone();
                Box::pin(async move {
                    session_ref
                        .undo(message_id)
                        .await
                        .map_err(|error| match error {
                            crate::agent::undo::UndoError::Transport(failure) => {
                                crate::error::AgentError::from_transport_failure(failure)
                            }
                            other => crate::error::AgentError::Internal(other.to_string()),
                        })
                })
            },
        )
        .await
        .map_err(|error| error.into_agent_error());
    #[cfg(not(feature = "remote"))]
    let result = match session_ref_for_session(state, &session_id).await {
        Some(session_ref) => session_ref
            .undo(message_id.to_string())
            .await
            .map_err(|error| crate::error::AgentError::Internal(error.to_string())),
        None => Err(crate::error::AgentError::SessionNotFound {
            session_id: session_id.clone(),
        }),
    };

    match result {
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

    #[cfg(feature = "remote")]
    let result = state
        .agent
        .execute_session_operation(
            &session_id,
            crate::agent::handle::session_operation::SessionOperation::Redo,
            |session_ref| {
                Box::pin(async {
                    session_ref.redo().await.map_err(|error| match error {
                        crate::agent::undo::UndoError::Transport(failure) => {
                            crate::error::AgentError::from_transport_failure(failure)
                        }
                        other => crate::error::AgentError::Internal(other.to_string()),
                    })
                })
            },
        )
        .await
        .map_err(|error| error.into_agent_error());
    #[cfg(not(feature = "remote"))]
    let result = match session_ref_for_session(state, &session_id).await {
        Some(session_ref) => session_ref
            .redo()
            .await
            .map_err(|error| crate::error::AgentError::Internal(error.to_string())),
        None => Err(crate::error::AgentError::SessionNotFound {
            session_id: session_id.clone(),
        }),
    };

    match result {
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
    session_id: Option<&str>,
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

    let tx = if let Some(session_id) = session_id {
        crate::elicitation::take_pending_elicitation_sender_for_session_with_profiles(
            state.agent.as_ref(),
            state.profiles.as_ref(),
            session_id,
            elicitation_id,
        )
        .await
    } else {
        crate::elicitation::take_pending_elicitation_sender(state.agent.as_ref(), elicitation_id)
            .await
    };

    if let Some(tx) = tx {
        if tx.send(response).is_err() {
            log::warn!(
                "Elicitation response receiver dropped for elicitation_id={}",
                elicitation_id
            );
        }
    } else {
        log::warn!(
            "No pending elicitation found for elicitation_id={} session_id={:?} (checked session runtime, primary, and delegates)",
            elicitation_id,
            session_id
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
    requested_session_id: Option<&str>,
    mode: &str,
    tx: &mpsc::Sender<String>,
) {
    match mode.parse::<AgentMode>() {
        Ok(new_mode) => {
            let session_id = if let Some(session_id) = requested_session_id {
                Some(session_id.to_string())
            } else {
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
                #[cfg(feature = "remote")]
                let result = state
                    .agent
                    .execute_session_operation(
                        session_id,
                        crate::agent::handle::session_operation::SessionOperation::SetMode,
                        |session_ref| Box::pin(session_ref.set_mode(new_mode)),
                    )
                    .await
                    .map_err(|error| error.into_agent_error());
                #[cfg(not(feature = "remote"))]
                let result = match session_ref_for_session(state, session_id).await {
                    Some(session_ref) => session_ref.set_mode(new_mode).await,
                    None => Err(crate::error::AgentError::SessionNotFound {
                        session_id: session_id.clone(),
                    }),
                };
                match result {
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
    requested_session_id: Option<&str>,
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

    let session_id = if let Some(session_id) = requested_session_id {
        Some(session_id.to_string())
    } else {
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
        #[cfg(feature = "remote")]
        let result = state
            .agent
            .execute_session_operation(
                session_id,
                crate::agent::handle::session_operation::SessionOperation::SetReasoningEffort,
                |session_ref| Box::pin(session_ref.set_reasoning_effort(effort)),
            )
            .await
            .map_err(|error| error.into_agent_error());
        #[cfg(not(feature = "remote"))]
        let result = match session_ref_for_session(state, session_id).await {
            Some(session_ref) => session_ref.set_reasoning_effort(effort).await,
            None => Err(crate::error::AgentError::SessionNotFound {
                session_id: session_id.clone(),
            }),
        };
        match result {
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

    let workspace =
        match get_or_create_workspace_with_timeout(&state.workspace_manager, root.clone()).await {
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
