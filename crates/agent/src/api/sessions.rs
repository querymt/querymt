use super::session::AgentSession;
use crate::acp::protocol::{
    ListSessionsRequest as AcpListSessionsRequest, ListSessionsResponse as AcpListSessionsResponse,
    LoadSessionRequest, Meta, NewSessionRequest, SessionId, SessionInfo,
};
use crate::agent::LocalAgentHandle;
use crate::agent::messages::SessionRuntimeStatus;
pub use crate::session::lifecycle::{
    ArchiveFilter, ResolutionFilter, SessionLifecycle, SessionLifecycleFilter,
    SessionLifecyclePatch,
};
use crate::session::load_snapshot::{SessionLoadSnapshot, load_session_snapshot};
use crate::session::projection::{SessionListItem, SessionScope, ViewStore};
use crate::session::store::SessionStore;
use anyhow::{Result, anyhow};
#[cfg(feature = "remote")]
use futures_util::StreamExt;
use querymt::chat::FinishReason;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(feature = "remote")]
use std::collections::{BTreeMap, HashSet};
#[cfg(feature = "remote")]
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(feature = "remote")]
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use typeshare::typeshare;

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AcpSessionLifecycleFilters {
    resolution: ResolutionFilter,
    archive: ArchiveFilter,
    #[serde(default, deserialize_with = "deserialize_filter_pinned")]
    pinned: Option<bool>,
}

fn deserialize_filter_pinned<'de, D>(deserializer: D) -> std::result::Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    bool::deserialize(deserializer).map(Some)
}

fn acp_session_filters(
    meta: Option<&Meta>,
) -> std::result::Result<(SessionLifecycleFilter, Option<bool>), AcpSessionListError> {
    let Some(querymt) = meta.and_then(|meta| meta.get("querymt")) else {
        return Ok((SessionLifecycleFilter::default(), None));
    };
    let Some(filters) = querymt
        .as_object()
        .ok_or_else(|| anyhow!("_meta.querymt must be an object"))?
        .get("sessionFilters")
    else {
        return Ok((SessionLifecycleFilter::default(), None));
    };
    let filters: AcpSessionLifecycleFilters = serde_json::from_value(filters.clone())
        .map_err(|error| anyhow!("invalid _meta.querymt.sessionFilters: {error}"))?;
    Ok((
        SessionLifecycleFilter {
            resolution: filters.resolution,
            archive: filters.archive,
        },
        filters.pinned,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionListMode {
    #[default]
    Browse,
    Group,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionMode {
    #[default]
    None,
    Bookmarks,
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListSessionsOptions {
    #[serde(default)]
    pub mode: SessionListMode,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub session_scope: Option<SessionScope>,
    #[serde(default)]
    pub remote: RemoteSessionMode,
}

/// Explicit transport connectivity for a remote session (plan §12).
///
/// Local sessions carry no transport connectivity and omit the field. The
/// legacy `attached` flag remains and is derived from this state
/// (`attached = connection_state == connected`) for compatibility.
#[typeshare]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSessionConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub parent_session_id: Option<String>,
    pub fork_origin: Option<String>,
    pub session_kind: Option<String>,
    pub has_children: bool,
    #[typeshare(serialized_as = "number")]
    pub fork_count: u64,
    pub node: Option<String>,
    pub node_id: Option<String>,
    pub attached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_state: Option<RemoteSessionConnectionState>,
    pub runtime_state: Option<String>,
    /// Managed-workspace view; absent for local sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::session::projection::SessionWorkspaceView>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub cwd: Option<String>,
    pub sessions: Vec<SessionSummary>,
    pub latest_activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "number")]
    pub total_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListPage {
    pub groups: Vec<SessionGroup>,
    pub next_cursor: Option<String>,
    pub total_count: u64,
}

#[derive(Debug, Clone)]
pub struct AcpSessionListPage {
    pub sessions: Vec<SessionInfo>,
    pub next_cursor: Option<String>,
    /// Local query total, plus matches examined in the current bounded remote page.
    /// ACP responses omit this count; remote traversal uses `next_cursor` instead.
    pub total_count: u64,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AcpSessionListError {
    #[error("invalid session list cursor")]
    InvalidCursor,
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

/// A raw offset is valid only for the live or bookmark list that produced it.
#[cfg(feature = "remote")]
#[derive(Debug, Clone, Copy, Default)]
struct AcpRemoteCursor {
    node: usize,
    offset: u32,
    source: Option<AcpRemoteSource>,
}

#[cfg(feature = "remote")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpRemoteSource {
    Live,
    Bookmarks,
}

#[cfg(feature = "remote")]
impl AcpRemoteCursor {
    /// Restart after a source switch or an untagged legacy continuation.
    fn offset_for(self, source: AcpRemoteSource) -> u32 {
        if self.source == Some(source) {
            self.offset
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AcpSessionCursor(i64);

impl AcpSessionCursor {
    fn parse(cursor: Option<&str>) -> std::result::Result<Option<Self>, AcpSessionListError> {
        cursor
            .map(|cursor| {
                cursor
                    .parse::<i64>()
                    .ok()
                    .filter(|offset| *offset >= 0)
                    .map(Self)
                    .ok_or(AcpSessionListError::InvalidCursor)
            })
            .transpose()
    }

    fn into_string(self) -> String {
        self.0.to_string()
    }
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    #[typeshare(serialized_as = "number")]
    #[serde(rename = "messageCount")]
    pub message_count: u32,
    #[typeshare(serialized_as = "number")]
    #[serde(rename = "userMessageCount")]
    pub user_message_count: u32,
    #[serde(rename = "hasErrors")]
    pub has_errors: bool,
    #[serde(rename = "runtimeStatus")]
    pub runtime_status: SessionRuntimeStatus,
}

impl SessionMeta {
    fn to_acp_meta(&self) -> Meta {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionChildrenPage {
    pub parent_session_id: String,
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<String>,
    pub total_count: u64,
}

/// A session summary paired with authoritative lifecycle metadata when available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummaryWithLifecycle {
    #[serde(flatten)]
    pub summary: SessionSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<SessionLifecycle>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroupWithLifecycle {
    pub cwd: Option<String>,
    pub sessions: Vec<SessionSummaryWithLifecycle>,
    pub latest_activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListPageWithLifecycle {
    pub groups: Vec<SessionGroupWithLifecycle>,
    pub next_cursor: Option<String>,
    pub total_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionChildrenPageWithLifecycle {
    pub parent_session_id: String,
    pub sessions: Vec<SessionSummaryWithLifecycle>,
    pub next_cursor: Option<String>,
    pub total_count: u64,
}

pub struct AgentLoadedSession {
    pub session: AgentSession,
    pub snapshot: SessionLoadSnapshot,
}

pub struct AgentSessions {
    agent: Arc<LocalAgentHandle>,
    view_store: Arc<dyn ViewStore>,
    session_store: Arc<dyn SessionStore>,
    default_cwd: Option<PathBuf>,
}

#[cfg(feature = "remote")]
const REMOTE_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(feature = "remote")]
const REMOTE_LIFECYCLE_CONCURRENCY: usize = 8;

#[cfg(feature = "remote")]
async fn load_remote_lifecycles_for_summaries<M, Find, FindFuture, Fetch, FetchFuture>(
    summaries: &[SessionSummary],
    find_manager: Find,
    fetch_lifecycle: Fetch,
) -> HashMap<String, Option<SessionLifecycle>>
where
    M: Clone + Send + Sync + 'static,
    Find: Fn(String) -> FindFuture + Send + Sync + 'static,
    FindFuture: Future<Output = Option<M>> + Send + 'static,
    Fetch: Fn(M, String) -> FetchFuture + Send + Sync + 'static,
    FetchFuture: Future<Output = Option<SessionLifecycle>> + Send + 'static,
{
    let mut node_sessions = BTreeMap::<String, Vec<String>>::new();
    let mut seen_session_ids = HashSet::new();
    for summary in summaries {
        if let Some(node_id) = &summary.node_id
            && seen_session_ids.insert(summary.session_id.clone())
        {
            node_sessions
                .entry(node_id.clone())
                .or_default()
                .push(summary.session_id.clone());
        }
    }

    let mut lifecycles_by_id = node_sessions
        .values()
        .flatten()
        .cloned()
        .map(|session_id| (session_id, None))
        .collect::<HashMap<_, _>>();
    let find_manager = Arc::new(find_manager);
    let fetch_lifecycle = Arc::new(fetch_lifecycle);
    let mut node_requests =
        futures_util::stream::iter(node_sessions.into_iter().map(|(node_id, session_ids)| {
            let find_manager = Arc::clone(&find_manager);
            let fetch_lifecycle = Arc::clone(&fetch_lifecycle);
            async move {
                let deadline = tokio::time::Instant::now() + REMOTE_LIFECYCLE_TIMEOUT;
                let manager = match tokio::time::timeout_at(deadline, find_manager(node_id)).await {
                    Ok(Some(manager)) => manager,
                    Ok(None) | Err(_) => return HashMap::new(),
                };

                let mut session_requests =
                    futures_util::stream::iter(session_ids.into_iter().map(|session_id| {
                        let fetch_lifecycle = Arc::clone(&fetch_lifecycle);
                        let manager = manager.clone();
                        async move {
                            let lifecycle = fetch_lifecycle(manager, session_id.clone()).await;
                            (session_id, lifecycle)
                        }
                    }))
                    .buffer_unordered(REMOTE_LIFECYCLE_CONCURRENCY);
                let mut node_lifecycles = HashMap::new();
                while let Ok(Some((session_id, lifecycle))) =
                    tokio::time::timeout_at(deadline, session_requests.next()).await
                {
                    node_lifecycles.insert(session_id, lifecycle);
                }
                node_lifecycles
            }
        }))
        .buffer_unordered(REMOTE_LIFECYCLE_CONCURRENCY);
    while let Some(node_lifecycles) = node_requests.next().await {
        lifecycles_by_id.extend(node_lifecycles);
    }
    lifecycles_by_id
}

impl AgentSessions {
    pub(crate) fn new(
        agent: Arc<LocalAgentHandle>,
        view_store: Arc<dyn ViewStore>,
        session_store: Arc<dyn SessionStore>,
        default_cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            agent,
            view_store,
            session_store,
            default_cwd,
        }
    }

    pub async fn list(&self, options: ListSessionsOptions) -> Result<SessionListPage> {
        let view_store = self.view_store()?;
        let ListSessionsOptions {
            mode,
            cursor,
            limit,
            cwd,
            query,
            session_scope,
            remote,
        } = options;

        let page_limit = limit.unwrap_or(20).clamp(1, 200) as usize;
        let session_scope = session_scope.unwrap_or_default();

        let mut page = match mode {
            SessionListMode::Group => {
                let cwd_value = match cwd.as_deref() {
                    Some("__none__") => None,
                    _ => cwd.map(normalize_group_cwd),
                };
                let (group, total) = view_store
                    .list_group_sessions(cwd_value, cursor, page_limit, session_scope)
                    .await?;
                SessionListPage {
                    next_cursor: group.next_cursor.clone(),
                    total_count: total as u64,
                    groups: vec![group.into()],
                }
            }
            SessionListMode::Search => {
                let (groups, next_cursor, total) = view_store
                    .search_sessions(query.unwrap_or_default(), cursor, page_limit, session_scope)
                    .await?;
                SessionListPage {
                    groups: groups.into_iter().map(Into::into).collect(),
                    next_cursor,
                    total_count: total as u64,
                }
            }
            SessionListMode::Browse => {
                let (groups, next_cursor, total) = view_store
                    .browse_session_groups(cursor, page_limit, 10, session_scope)
                    .await?;
                SessionListPage {
                    groups: groups.into_iter().map(Into::into).collect(),
                    next_cursor,
                    total_count: total as u64,
                }
            }
        };

        match remote {
            RemoteSessionMode::None => {}
            RemoteSessionMode::Bookmarks => {
                self.merge_remote_bookmarks(&mut page.groups).await;
            }
            RemoteSessionMode::Live => {
                self.merge_remote_bookmarks(&mut page.groups).await;
                self.merge_remote_live(&mut page.groups).await;
            }
        }

        Ok(page)
    }

    /// List sessions with lifecycle filtering and per-row authoritative metadata.
    pub async fn list_with_lifecycle(
        &self,
        options: ListSessionsOptions,
        lifecycle: SessionLifecycleFilter,
    ) -> Result<SessionListPageWithLifecycle> {
        let page = if lifecycle.is_filtered() {
            if options.remote != RemoteSessionMode::None {
                return Err(anyhow!(
                    "Lifecycle-filtered API listing requires a complete local owner query; remote bookmark/live merge is unsupported"
                ));
            }
            let view_store = self.view_store()?;
            if !view_store.supports_session_lifecycle()
                || !view_store.supports_session_lifecycle_filtering()
            {
                return Err(anyhow!(
                    "Session lifecycle filtering is unsupported by this view store"
                ));
            }
            let page_limit = options.limit.unwrap_or(20).clamp(1, 200) as usize;
            let scope = options.session_scope.unwrap_or_default();
            match options.mode {
                SessionListMode::Group => {
                    let cwd = match options.cwd.as_deref() {
                        Some("__none__") => None,
                        _ => options.cwd.map(normalize_group_cwd),
                    };
                    let (group, total) = view_store
                        .list_group_sessions_with_lifecycle(
                            cwd,
                            options.cursor,
                            page_limit,
                            scope,
                            lifecycle,
                        )
                        .await?;
                    SessionListPage {
                        next_cursor: group.next_cursor.clone(),
                        total_count: total as u64,
                        groups: vec![group.into()],
                    }
                }
                SessionListMode::Search => {
                    let (groups, next_cursor, total) = view_store
                        .search_sessions_with_lifecycle(
                            options.query.unwrap_or_default(),
                            options.cursor,
                            page_limit,
                            scope,
                            lifecycle,
                        )
                        .await?;
                    SessionListPage {
                        groups: groups.into_iter().map(Into::into).collect(),
                        next_cursor,
                        total_count: total as u64,
                    }
                }
                SessionListMode::Browse => {
                    let (groups, next_cursor, total) = view_store
                        .browse_session_groups_with_lifecycle(
                            options.cursor,
                            page_limit,
                            10,
                            scope,
                            lifecycle,
                        )
                        .await?;
                    SessionListPage {
                        groups: groups.into_iter().map(Into::into).collect(),
                        next_cursor,
                        total_count: total as u64,
                    }
                }
            }
        } else {
            self.list(options).await?
        };
        self.enrich_session_page_with_lifecycle(page).await
    }

    pub async fn browse_with_lifecycle(
        &self,
        mut options: ListSessionsOptions,
        lifecycle: SessionLifecycleFilter,
    ) -> Result<SessionListPageWithLifecycle> {
        options.mode = SessionListMode::Browse;
        self.list_with_lifecycle(options, lifecycle).await
    }

    pub async fn search_with_lifecycle(
        &self,
        query: impl Into<String>,
        mut options: ListSessionsOptions,
        lifecycle: SessionLifecycleFilter,
    ) -> Result<SessionListPageWithLifecycle> {
        options.mode = SessionListMode::Search;
        options.query = Some(query.into());
        self.list_with_lifecycle(options, lifecycle).await
    }

    pub async fn list_group_with_lifecycle(
        &self,
        cwd: Option<String>,
        mut options: ListSessionsOptions,
        lifecycle: SessionLifecycleFilter,
    ) -> Result<SessionListPageWithLifecycle> {
        options.mode = SessionListMode::Group;
        options.cwd = cwd;
        self.list_with_lifecycle(options, lifecycle).await
    }

    pub async fn children_with_lifecycle(
        &self,
        parent_session_id: impl Into<String>,
        cursor: Option<String>,
        limit: Option<u32>,
        lifecycle: SessionLifecycleFilter,
    ) -> Result<SessionChildrenPageWithLifecycle> {
        let parent_session_id = parent_session_id.into();
        let page_limit = limit.unwrap_or(20).clamp(1, 200) as usize;
        let view_store = self.view_store()?;
        let (group, total) = if lifecycle.is_filtered() {
            if !view_store.supports_session_lifecycle()
                || !view_store.supports_session_lifecycle_filtering()
            {
                return Err(anyhow!(
                    "Session lifecycle filtering is unsupported by this view store"
                ));
            }
            view_store
                .list_session_children_with_lifecycle(
                    parent_session_id.clone(),
                    cursor,
                    page_limit,
                    lifecycle,
                )
                .await?
        } else {
            view_store
                .list_session_children(parent_session_id.clone(), cursor, page_limit)
                .await?
        };
        let summaries: Vec<SessionSummary> = group.sessions.into_iter().map(Into::into).collect();
        let lifecycle_by_id = self
            .enrich_summaries_with_lifecycle(summaries.clone())
            .await?;
        let sessions = summaries
            .into_iter()
            .map(|summary| SessionSummaryWithLifecycle {
                lifecycle: lifecycle_by_id
                    .get(&summary.session_id)
                    .and_then(Clone::clone),
                summary,
            })
            .collect();
        Ok(SessionChildrenPageWithLifecycle {
            parent_session_id,
            sessions,
            next_cursor: group.next_cursor,
            total_count: total as u64,
        })
    }

    pub async fn get_session_lifecycle(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<SessionLifecycle> {
        self.agent
            .get_session_lifecycle_unloaded(session_id.as_ref())
            .await
            .map_err(anyhow::Error::from)
    }

    pub async fn set_session_lifecycle(
        &self,
        session_id: impl AsRef<str>,
        patch: crate::session::lifecycle::SessionLifecyclePatch,
    ) -> Result<SessionLifecycle> {
        self.agent
            .patch_session_lifecycle_unloaded(session_id.as_ref(), &patch)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn enrich_session_page_with_lifecycle(
        &self,
        page: SessionListPage,
    ) -> Result<SessionListPageWithLifecycle> {
        let summaries: Vec<_> = page
            .groups
            .iter()
            .flat_map(|group| group.sessions.iter().cloned())
            .collect();
        let lifecycle_by_id = self
            .enrich_summaries_with_lifecycle(summaries.clone())
            .await?;
        let groups = page
            .groups
            .into_iter()
            .map(|group| SessionGroupWithLifecycle {
                cwd: group.cwd,
                sessions: group
                    .sessions
                    .into_iter()
                    .map(|summary| SessionSummaryWithLifecycle {
                        lifecycle: lifecycle_by_id
                            .get(&summary.session_id)
                            .and_then(|state| state.clone()),
                        summary,
                    })
                    .collect(),
                latest_activity: group.latest_activity,
                total_count: group.total_count,
                next_cursor: group.next_cursor,
            })
            .collect();
        Ok(SessionListPageWithLifecycle {
            groups,
            next_cursor: page.next_cursor,
            total_count: page.total_count,
        })
    }

    async fn enrich_summaries_with_lifecycle(
        &self,
        summaries: Vec<SessionSummary>,
    ) -> Result<HashMap<String, Option<SessionLifecycle>>> {
        let mut lifecycle_by_id = HashMap::new();
        let session_store = self.agent.config.provider.history_store();
        let local_candidate_ids = summaries
            .iter()
            .filter(|summary| summary.node_id.is_none())
            .map(|summary| summary.session_id.clone())
            .collect::<Vec<_>>();
        let locations = session_store
            .resolve_session_locations(&local_candidate_ids)
            .await?;
        let mut local_ids = Vec::new();
        for session_id in &local_candidate_ids {
            let Some(location) = locations.get(session_id) else {
                continue;
            };
            match location {
                crate::session::location::SessionLocation::Local => {
                    local_ids.push(session_id.clone());
                }
                crate::session::location::SessionLocation::Conflict { .. } => {
                    return Err(anyhow!(
                        "Session location conflict: {session_id} exists locally and as a remote bookmark"
                    ));
                }
                crate::session::location::SessionLocation::Remote { .. }
                | crate::session::location::SessionLocation::NotFound => {}
            }
        }

        let view_store = self.view_store()?;
        if view_store.supports_session_lifecycle() && !local_ids.is_empty() {
            lifecycle_by_id.extend(
                view_store
                    .get_session_list_lifecycles(&local_ids)
                    .await?
                    .into_iter()
                    .map(|(id, state)| (id, Some(state))),
            );
        }
        #[cfg(feature = "remote")]
        {
            let find_agent = self.agent.clone();
            let fetch_agent = self.agent.clone();
            let remote_states = load_remote_lifecycles_for_summaries(
                &summaries,
                move |node_id| {
                    let agent = find_agent.clone();
                    async move { agent.find_node_manager(&node_id).await.ok() }
                },
                move |manager, session_id| {
                    let agent = fetch_agent.clone();
                    async move {
                        agent
                            .get_remote_session_lifecycle(&manager, session_id)
                            .await
                            .ok()
                    }
                },
            )
            .await;
            lifecycle_by_id.extend(remote_states);
        }
        #[cfg(not(feature = "remote"))]
        let _ = summaries;
        Ok(lifecycle_by_id)
    }

    pub async fn list_acp(
        &self,
        request: AcpListSessionsRequest,
    ) -> Result<AcpListSessionsResponse> {
        let page = self.list_for_acp(request).await?;
        Ok(AcpListSessionsResponse::new(page.sessions).next_cursor(page.next_cursor))
    }

    pub async fn list_for_acp(
        &self,
        request: AcpListSessionsRequest,
    ) -> Result<AcpSessionListPage> {
        Self::list_for_acp_with_runtime(&self.agent, self.view_store()?, request)
            .await
            .map_err(anyhow::Error::from)
    }

    pub(crate) async fn list_for_acp_with_runtime(
        agent: &LocalAgentHandle,
        view_store: Arc<dyn ViewStore>,
        request: AcpListSessionsRequest,
    ) -> std::result::Result<AcpSessionListPage, AcpSessionListError> {
        let (lifecycle, pinned) = acp_session_filters(request.meta.as_ref())?;
        #[cfg(feature = "remote")]
        let remote_nodes = acp_remote_node_ids(request.meta.as_ref());
        #[cfg(feature = "remote")]
        if pinned.is_some() && !remote_nodes.is_empty() {
            return Err(
                anyhow!("pinned session filters cannot be combined with remoteNodeIds").into(),
            );
        }
        #[cfg(feature = "remote")]
        let remote_cursor = request
            .cursor
            .as_deref()
            .filter(|cursor| cursor.starts_with("remote:"))
            .map(parse_acp_remote_cursor)
            .transpose()?;
        #[cfg(feature = "remote")]
        if remote_cursor.is_some() && remote_nodes.is_empty() {
            return Err(AcpSessionListError::InvalidCursor);
        }
        #[cfg(feature = "remote")]
        let remote_cwd = request
            .cwd
            .as_ref()
            .map(|cwd| normalize_group_cwd(cwd.display().to_string()));
        #[cfg(feature = "remote")]
        let remote_scope = acp_session_scope_from_meta(request.meta.as_ref());
        #[cfg(feature = "remote")]
        if let Some(cursor) = remote_cursor {
            return Self::list_acp_remote_page(
                agent,
                &remote_nodes,
                remote_cwd.as_deref(),
                remote_scope,
                lifecycle,
                cursor,
            )
            .await;
        }

        let mut page = if pinned.is_some() {
            Self::list_for_acp_from_view_store_with_pins(
                view_store.clone(),
                request,
                lifecycle,
                pinned,
            )
            .await?
        } else if lifecycle.is_filtered() {
            Self::list_for_acp_from_view_store_with_lifecycle(
                view_store.clone(),
                request,
                lifecycle,
            )
            .await?
        } else {
            Self::list_for_acp_from_view_store(view_store.clone(), request).await?
        };
        Self::hydrate_acp_session_meta(agent, view_store, &mut page.sessions).await?;
        #[cfg(feature = "remote")]
        if !remote_nodes.is_empty() && page.next_cursor.is_none() {
            let remote_page = Self::list_acp_remote_page(
                agent,
                &remote_nodes,
                remote_cwd.as_deref(),
                remote_scope,
                lifecycle,
                AcpRemoteCursor::default(),
            )
            .await?;
            page.sessions.extend(remote_page.sessions);
            page.next_cursor = remote_page.next_cursor;
            page.total_count += remote_page.total_count;
        }
        Ok(page)
    }

    #[cfg(feature = "remote")]
    /// Fetch one bounded owner page, keeping its raw offset even when filters
    /// remove every entry. Further peers and pages are visited by continuation.
    async fn list_acp_remote_page(
        agent: &LocalAgentHandle,
        nodes: &[String],
        cwd: Option<&str>,
        scope: SessionScope,
        lifecycle: SessionLifecycleFilter,
        cursor: AcpRemoteCursor,
    ) -> std::result::Result<AcpSessionListPage, AcpSessionListError> {
        let node_id = nodes
            .get(cursor.node)
            .ok_or(AcpSessionListError::InvalidCursor)?;
        let offset = cursor.offset_for(AcpRemoteSource::Live);
        let limit = if cwd.is_some() { 10 } else { 100 };
        let store = agent.config.provider.history_store();
        let bookmarks = store
            .list_remote_session_bookmarks()
            .await
            .map_err(anyhow::Error::from)?;
        let bookmark_owners: HashMap<_, _> = bookmarks
            .iter()
            .map(|bookmark| (bookmark.session_id.as_str(), bookmark.node_id.as_str()))
            .collect();
        let local_ids: std::collections::HashSet<_> = store
            .list_sessions()
            .await
            .map_err(anyhow::Error::from)?
            .into_iter()
            .map(|session| session.public_id)
            .collect();

        // Explicit lifecycle predicates run at the owner; otherwise retain raw-page
        // traversal for workspace and relationship filtering.
        let (owner_cwd, owner_scope) = if lifecycle.is_filtered() {
            (cwd.map(str::to_owned), scope)
        } else {
            (None, SessionScope::All)
        };
        let owner_page = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let manager = agent.find_node_manager(node_id).await?;
            match agent
                .list_remote_sessions_with_lifecycle(
                    &manager,
                    Some(offset),
                    Some(limit),
                    owner_cwd.clone(),
                    owner_scope,
                    lifecycle,
                )
                .await
            {
                Ok(response) => {
                    let lifecycle_by_id: HashMap<String, SessionLifecycle> = response
                        .sessions
                        .iter()
                        .map(|row| (row.session.session_id.clone(), row.lifecycle.clone()))
                        .collect();
                    let entries = response
                        .sessions
                        .into_iter()
                        .map(|row| row.session)
                        .collect();
                    Ok((entries, response.next_offset, Some(lifecycle_by_id)))
                }
                Err(error) if lifecycle.is_filtered() => Err(error),
                Err(_) => agent
                    .list_remote_sessions(&manager, Some(offset), Some(limit))
                    .await
                    .map(|response| (response.sessions, response.next_offset, None)),
            }
        })
        .await;
        let (entries, next_offset, live, remote_lifecycles) = match owner_page {
            Ok(Ok((entries, next_offset, remote_lifecycles))) => {
                let response = crate::agent::remote::ListRemoteSessionsResponse {
                    sessions: entries.clone(),
                    next_offset,
                    total_count: 0,
                };
                validate_acp_remote_page(&response, offset, limit)?;
                (entries, next_offset, true, remote_lifecycles)
            }
            Ok(Err(error)) if lifecycle.is_filtered() => {
                return Err(anyhow!(
                    "Remote session owner cannot provide lifecycle-filtered results: {error}"
                )
                .into());
            }
            Err(error) if lifecycle.is_filtered() => {
                return Err(anyhow!(
                    "Remote session owner is unavailable for lifecycle-filtered results: {error}"
                )
                .into());
            }
            error => {
                log::debug!("Unable to list sessions from peer {}: {:?}", node_id, error);
                let peer_bookmarks: Vec<_> = bookmarks
                    .iter()
                    .filter(|bookmark| bookmark.node_id == *node_id)
                    .collect();
                let start = cursor.offset_for(AcpRemoteSource::Bookmarks) as usize;
                let entries: Vec<_> = peer_bookmarks
                    .iter()
                    .skip(start)
                    .take(limit as usize)
                    .map(|bookmark| crate::agent::remote::RemoteSessionSnapshot {
                        session_id: bookmark.session_id.clone(),
                        actor_id: 0,
                        cwd: bookmark.cwd.clone(),
                        created_at: bookmark.created_at,
                        updated_at: None,
                        title: bookmark.title.clone(),
                        profile_id: None,
                        profile_label: None,
                        peer_label: bookmark.peer_label.clone(),
                        runtime_state: None,
                        parent_session_id: None,
                        fork_origin: None,
                    })
                    .collect();
                let end = start.saturating_add(entries.len());
                let next = if end < peer_bookmarks.len() {
                    Some(u32::try_from(end).map_err(|_| AcpSessionListError::InvalidCursor)?)
                } else {
                    None
                };
                (entries, next, false, None)
            }
        };
        let source = if live { "live" } else { "bookmarks" };
        let next_cursor = next_offset
            .map(|offset| format!("remote:{}:{offset}:{source}", cursor.node))
            .or_else(|| {
                (cursor.node + 1 < nodes.len()).then(|| format!("remote:{}:0", cursor.node + 1))
            });
        let mut sessions = Vec::new();
        for entry in entries {
            let same_cwd = cwd.is_none_or(|requested| {
                entry
                    .cwd
                    .as_deref()
                    .map(|cwd| normalize_group_cwd(cwd.to_string()))
                    .as_deref()
                    == Some(requested)
            });
            let same_scope = match scope {
                SessionScope::All => true,
                SessionScope::Root => entry.parent_session_id.is_none(),
                SessionScope::Forks => entry
                    .fork_origin
                    .as_deref()
                    .is_some_and(crate::session::domain::ForkOrigin::is_visible_fork),
                SessionScope::Delegates => entry.fork_origin.as_deref() == Some("delegation"),
                SessionScope::Children => entry.parent_session_id.is_some(),
            };
            // Durable ownership wins over a peer advertising a conflicting ID.
            if !same_cwd
                || !same_scope
                || local_ids.contains(&entry.session_id)
                || bookmark_owners
                    .get(entry.session_id.as_str())
                    .is_some_and(|owner| *owner != node_id.as_str())
            {
                continue;
            }
            let lifecycle_metadata = remote_lifecycles
                .as_ref()
                .and_then(|lifecycles| lifecycles.get(&entry.session_id))
                .cloned();
            let mut info = SessionInfo::new(
                SessionId::from(entry.session_id),
                entry.cwd.map(PathBuf::from).unwrap_or_default(),
            );
            info.title = entry.title;
            info.updated_at = time::OffsetDateTime::from_unix_timestamp(
                entry.updated_at.unwrap_or(entry.created_at),
            )
            .ok()
            .and_then(|ts| ts.format(&Rfc3339).ok());
            let mut meta = Meta::new();
            meta.insert("location".into(), serde_json::json!("remote"));
            meta.insert("nodeId".into(), serde_json::json!(node_id));
            meta.insert("nodeLabel".into(), serde_json::json!(entry.peer_label));
            meta.insert("profileId".into(), serde_json::json!(entry.profile_id));
            meta.insert(
                "profileLabel".into(),
                serde_json::json!(entry.profile_label),
            );
            meta.insert(
                "connectionState".into(),
                serde_json::json!(if live { "available" } else { "disconnected" }),
            );
            if let Some(parent) = entry.parent_session_id {
                meta.insert("parentSessionId".into(), serde_json::json!(parent));
            }
            if let Some(origin) = entry.fork_origin {
                meta.insert("forkOrigin".into(), serde_json::json!(origin));
            }
            if let Some(lifecycle) = lifecycle_metadata.as_ref() {
                insert_acp_lifecycle_meta(&mut meta, lifecycle);
            }
            info.meta = Some(meta);
            sessions.push(info);
        }
        // ACP does not expose total_count. Counting remote matches exactly
        // would require the full-history traversal this pagination avoids.
        Ok(AcpSessionListPage {
            total_count: sessions.len() as u64,
            sessions,
            next_cursor,
        })
    }

    pub(crate) async fn list_for_acp_from_view_store(
        view_store: Arc<dyn ViewStore>,
        request: AcpListSessionsRequest,
    ) -> std::result::Result<AcpSessionListPage, AcpSessionListError> {
        Self::list_for_acp_from_view_store_with_lifecycle(
            view_store,
            request,
            SessionLifecycleFilter::default(),
        )
        .await
    }

    async fn list_for_acp_from_view_store_with_lifecycle(
        view_store: Arc<dyn ViewStore>,
        request: AcpListSessionsRequest,
        lifecycle: SessionLifecycleFilter,
    ) -> std::result::Result<AcpSessionListPage, AcpSessionListError> {
        Self::list_for_acp_from_view_store_with_pins(view_store, request, lifecycle, None).await
    }

    async fn list_for_acp_from_view_store_with_pins(
        view_store: Arc<dyn ViewStore>,
        request: AcpListSessionsRequest,
        lifecycle: SessionLifecycleFilter,
        pinned: Option<bool>,
    ) -> std::result::Result<AcpSessionListPage, AcpSessionListError> {
        let cursor = AcpSessionCursor::parse(request.cursor.as_deref())?;
        let requested_cwd = request
            .cwd
            .map(|cwd| normalize_group_cwd(cwd.display().to_string()));
        let session_scope = acp_session_scope_from_meta(request.meta.as_ref());
        // ACP workspace requests load incrementally; global discovery remains a larger flat page.
        let limit = if requested_cwd.is_some() { 10 } else { 100 };

        // `children` + `_meta.querymt.parentSessionId` pages one parent's
        // direct children (forks and worktree siblings; delegates stay hidden).
        if session_scope == SessionScope::Children
            && let Some(parent) = acp_parent_session_id_from_meta(request.meta.as_ref())
        {
            if pinned.is_some() {
                return Err(anyhow!("pin filters are unsupported for child listings").into());
            }
            let (group, total_count) = view_store
                .list_session_children_with_lifecycle(
                    parent,
                    cursor.map(AcpSessionCursor::into_string),
                    limit,
                    lifecycle,
                )
                .await
                .map_err(anyhow::Error::from)?;
            return Ok(AcpSessionListPage {
                sessions: group
                    .sessions
                    .into_iter()
                    .map(session_list_item_to_acp_info)
                    .collect(),
                next_cursor: group.next_cursor,
                total_count: total_count as u64,
            });
        }

        let (sessions, next_cursor, total_count) = view_store
            .list_session_items_with_pins(
                requested_cwd,
                cursor.map(AcpSessionCursor::into_string),
                limit,
                session_scope,
                lifecycle,
                pinned,
            )
            .await
            .map_err(anyhow::Error::from)?;

        Ok(AcpSessionListPage {
            sessions: sessions
                .into_iter()
                .map(session_list_item_to_acp_info)
                .collect(),
            next_cursor,
            total_count: total_count as u64,
        })
    }

    async fn hydrate_acp_session_meta(
        agent: &LocalAgentHandle,
        view_store: Arc<dyn ViewStore>,
        sessions: &mut [SessionInfo],
    ) -> Result<()> {
        let session_ids: Vec<String> = sessions
            .iter()
            .map(|info| info.session_id.to_string())
            .collect();
        if session_ids.is_empty() {
            return Ok(());
        }

        let session_store = agent.config.provider.history_store();
        let locations = session_store
            .resolve_session_locations(&session_ids)
            .await?;
        let mut local_session_ids = Vec::with_capacity(session_ids.len());
        for session_id in &session_ids {
            let Some(location) = locations.get(session_id) else {
                continue;
            };
            match location {
                crate::session::location::SessionLocation::Local => {
                    local_session_ids.push(session_id.clone());
                }
                crate::session::location::SessionLocation::Conflict { .. } => {
                    return Err(anyhow!(
                        "Session location conflict: {session_id} exists locally and as a remote bookmark"
                    ));
                }
                crate::session::location::SessionLocation::Remote { .. }
                | crate::session::location::SessionLocation::NotFound => {}
            }
        }

        let persisted_stats = view_store.get_session_list_meta_stats(&session_ids).await?;
        let lifecycles = if view_store.supports_session_lifecycle() && !local_session_ids.is_empty()
        {
            view_store
                .get_session_list_lifecycles(&local_session_ids)
                .await?
        } else {
            HashMap::new()
        };
        let pins = if session_store.supports_session_pins() && !local_session_ids.is_empty() {
            session_store.get_session_pins(&local_session_ids).await?
        } else {
            HashMap::new()
        };
        let actor_refs = {
            let registry = agent.registry.lock().await;
            registry.get_many(session_ids.iter().map(String::as_str))
        };
        let runtime_statuses = futures_util::future::join_all(actor_refs.into_iter().map(
            |(session_id, actor_ref)| async move {
                let status = actor_ref
                    .get_runtime_status()
                    .await
                    .unwrap_or(SessionRuntimeStatus::Idle);
                (session_id, status)
            },
        ))
        .await
        .into_iter()
        .collect::<HashMap<_, _>>();

        for info in sessions {
            let session_id = info.session_id.to_string();
            let stats = persisted_stats
                .get(&session_id)
                .cloned()
                .unwrap_or_default();
            let runtime_status = runtime_statuses
                .get(&session_id)
                .cloned()
                .unwrap_or(SessionRuntimeStatus::Idle);
            let has_errors = stats.last_finish_reason == Some(FinishReason::Error);
            let meta = SessionMeta {
                message_count: stats.message_count,
                user_message_count: stats.user_message_count,
                has_errors,
                runtime_status,
            };
            let mut acp_meta = info.meta.take().unwrap_or_default();
            acp_meta.extend(meta.to_acp_meta());
            if let Some(lifecycle) = lifecycles.get(&session_id) {
                insert_acp_lifecycle_meta(&mut acp_meta, lifecycle);
            }
            if let Some(pin) = pins.get(&session_id) {
                insert_acp_pin_meta(&mut acp_meta, pin);
            }
            info.meta = Some(acp_meta);
        }

        Ok(())
    }

    pub async fn browse(&self, options: ListSessionsOptions) -> Result<SessionListPage> {
        self.list(ListSessionsOptions {
            mode: SessionListMode::Browse,
            ..options
        })
        .await
    }

    pub async fn search(
        &self,
        query: impl Into<String>,
        options: ListSessionsOptions,
    ) -> Result<SessionListPage> {
        self.list(ListSessionsOptions {
            mode: SessionListMode::Search,
            query: Some(query.into()),
            ..options
        })
        .await
    }

    pub async fn list_group(
        &self,
        cwd: Option<String>,
        options: ListSessionsOptions,
    ) -> Result<SessionListPage> {
        self.list(ListSessionsOptions {
            mode: SessionListMode::Group,
            cwd,
            ..options
        })
        .await
    }

    pub async fn children(
        &self,
        parent_session_id: impl Into<String>,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> Result<SessionChildrenPage> {
        let parent_session_id = parent_session_id.into();
        let page_limit = limit.unwrap_or(20).clamp(1, 200) as usize;
        let view_store = self.view_store()?;
        let (group, total) = view_store
            .list_session_children(parent_session_id.clone(), cursor, page_limit)
            .await?;
        Ok(SessionChildrenPage {
            parent_session_id,
            sessions: group.sessions.into_iter().map(Into::into).collect(),
            next_cursor: group.next_cursor,
            total_count: total as u64,
        })
    }

    pub async fn create(&self, cwd: Option<PathBuf>) -> Result<AgentSession> {
        let request = match cwd.or_else(|| self.default_cwd.clone()) {
            Some(cwd) => NewSessionRequest::new(cwd),
            None => NewSessionRequest::new(PathBuf::new()),
        };
        let response = self
            .agent
            .new_session(request)
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(AgentSession::new(
            self.agent.clone(),
            response.session_id.to_string(),
        ))
    }

    pub async fn load(&self, session_id: impl AsRef<str>) -> Result<AgentSession> {
        let session_id = session_id.as_ref().to_string();
        self.agent
            .load_session(LoadSessionRequest::new(
                SessionId::from(session_id.clone()),
                PathBuf::new(),
            ))
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let workspace = self.durable_workspace_disclosure(&session_id).await?;
        Ok(AgentSession::new(self.agent.clone(), session_id).with_workspace(workspace))
    }

    /// The session's durable managed-workspace binding, disclosed in the same
    /// shape as `session/new`; `None` for local sessions or when no workspace
    /// service is configured.
    async fn durable_workspace_disclosure(
        &self,
        session_id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let Some(service) = self.agent.config.workspace_service() else {
            return Ok(None);
        };
        let binding = service
            .resolve_existing_session(session_id, None)
            .await
            .map_err(|e| anyhow!("failed to resolve workspace binding: {e}"))?;
        Ok(binding.map(|b| b.disclosure()))
    }

    pub async fn load_with_snapshot(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<AgentLoadedSession> {
        let session_id = session_id.as_ref().to_string();
        self.agent
            .load_session(LoadSessionRequest::new(
                SessionId::from(session_id.clone()),
                PathBuf::new(),
            ))
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        let snapshot = load_session_snapshot(&self.agent, self.view_store()?, &session_id).await?;
        let workspace = self.durable_workspace_disclosure(&session_id).await?;
        Ok(AgentLoadedSession {
            session: AgentSession::new(self.agent.clone(), session_id).with_workspace(workspace),
            snapshot,
        })
    }

    pub async fn delete(&self, session_id: impl AsRef<str>) -> Result<()> {
        let session_id = session_id.as_ref().to_string();
        let store = self.session_store();
        // Remove the durable remote bookmark first: if that fails, return
        // early with the session row intact instead of deleting the row and
        // leaving a dangling bookmark behind.
        store.remove_remote_session_bookmark(&session_id).await?;
        store.delete_session(&session_id).await?;
        self.agent.config.remove_run_lease_file(&session_id);
        if store.supports_session_pins() {
            crate::control::notifications::emit_session_pins_changed_notification(
                &self.agent,
                &session_id,
            )
            .await;
        }
        self.agent.clear_delegate_model_overrides(&session_id).await;
        #[cfg(feature = "remote")]
        {
            if self
                .agent
                .detach_remote_session_attachment(&session_id, true)
                .await
                .is_none()
            {
                self.agent.registry.lock().await.remove(&session_id);
            }
        }
        #[cfg(not(feature = "remote"))]
        {
            self.agent.registry.lock().await.remove(&session_id);
        }
        Ok(())
    }

    fn view_store(&self) -> Result<Arc<dyn ViewStore>> {
        Ok(self.view_store.clone())
    }

    fn session_store(&self) -> Arc<dyn SessionStore> {
        self.session_store.clone()
    }

    async fn merge_remote_bookmarks(&self, groups: &mut Vec<SessionGroup>) {
        #[cfg(not(feature = "remote"))]
        {
            let _ = groups;
        }

        #[cfg(feature = "remote")]
        {
            let bookmarks = match self.session_store().list_remote_session_bookmarks().await {
                Ok(bookmarks) => bookmarks,
                Err(err) => {
                    log::warn!("Failed to load remote session bookmarks: {}", err);
                    return;
                }
            };

            let bookmark_titles: std::collections::HashMap<String, String> = bookmarks
                .iter()
                .filter_map(|bookmark| {
                    bookmark
                        .title
                        .clone()
                        .map(|title| (bookmark.session_id.clone(), title))
                })
                .collect();

            let remote = {
                let registry = self.agent.registry.lock().await;
                registry.remote_sessions()
            };
            if !remote.is_empty() {
                let cwds: std::collections::HashMap<String, String> = {
                    let sessions = match self.session_store().list_sessions().await {
                        Ok(sessions) => sessions,
                        Err(err) => {
                            log::warn!(
                                "Failed to load session metadata for remote bookmarks: {}",
                                err
                            );
                            Vec::new()
                        }
                    };
                    sessions
                        .into_iter()
                        .filter_map(|session| {
                            session
                                .cwd
                                .map(|cwd| (session.public_id, cwd.display().to_string()))
                        })
                        .collect()
                };
                for (session_id, peer_label, remote_node_id) in remote {
                    let summary = SessionSummary {
                        session_id: session_id.clone(),
                        name: bookmark_titles.get(&session_id).cloned(),
                        cwd: cwds.get(&session_id).cloned(),
                        title: bookmark_titles.get(&session_id).cloned(),
                        created_at: None,
                        updated_at: None,
                        parent_session_id: None,
                        fork_origin: None,
                        session_kind: None,
                        has_children: false,
                        fork_count: 0,
                        node: Some(peer_label.clone()),
                        node_id: remote_node_id,
                        attached: Some(true),
                        connection_state: Some(RemoteSessionConnectionState::Connected),
                        runtime_state: None,
                        workspace: None,
                    };
                    push_group_session(groups, format!("remote::{}", peer_label), summary);
                }
            }

            if !bookmarks.is_empty() {
                let registry_ids: std::collections::HashSet<String> = {
                    let registry = self.agent.registry.lock().await;
                    registry.session_ids().into_iter().collect()
                };

                for bookmark in bookmarks {
                    if registry_ids.contains(&bookmark.session_id) {
                        continue;
                    }
                    // Connecting while an open/recovery attempt is in flight
                    // for this session (single-flight gate entry present).
                    let connecting = self.agent.remote_connect_in_flight(&bookmark.session_id);
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
                        connection_state: Some(if connecting {
                            RemoteSessionConnectionState::Connecting
                        } else {
                            RemoteSessionConnectionState::Disconnected
                        }),
                        runtime_state: Some("stopped".to_string()),
                        workspace: None,
                    };
                    push_group_session(groups, format!("remote::{}", bookmark.peer_label), summary);
                }
            }
        }
    }

    #[cfg(feature = "remote")]
    async fn list_remote_sessions_for_node(
        agent: Arc<LocalAgentHandle>,
        node_id: String,
    ) -> Option<Vec<crate::agent::remote::RemoteSessionSnapshot>> {
        let nm_ref = agent.find_node_manager(&node_id).await.ok()?;
        agent
            .list_remote_sessions(&nm_ref, None, None)
            .await
            .ok()
            .map(|response| response.sessions)
    }

    async fn merge_remote_live(&self, groups: &mut Vec<SessionGroup>) {
        #[cfg(not(feature = "remote"))]
        {
            let _ = groups;
        }

        #[cfg(feature = "remote")]
        {
            if self.agent.mesh().is_none() {
                return;
            }

            let attached_sessions: std::collections::HashSet<String> = {
                let registry = self.agent.registry.lock().await;
                registry
                    .remote_sessions()
                    .into_iter()
                    .map(|(id, _, _)| id)
                    .collect()
            };

            let node_infos = self.agent.list_remote_nodes().await;
            let node_id_by_label: std::collections::HashMap<String, String> = node_infos
                .into_iter()
                .map(|n| (n.hostname, n.node_id.to_string()))
                .collect();

            if node_id_by_label.is_empty() {
                return;
            }

            let peer_futures: Vec<_> = node_id_by_label
                .iter()
                .map(|(peer_label, node_id_str)| {
                    let peer_label = peer_label.clone();
                    let node_id_str = node_id_str.clone();
                    let agent = self.agent.clone();
                    async move {
                        let sessions =
                            Self::list_remote_sessions_for_node(agent, node_id_str.clone()).await?;
                        Some((peer_label, node_id_str, sessions))
                    }
                })
                .collect();

            let peer_results = futures_util::future::join_all(peer_futures).await;
            for result in peer_results.into_iter().flatten() {
                let (peer_label, node_id_str, sessions) = result;
                for session_info in sessions {
                    if attached_sessions.contains(&session_info.session_id) {
                        continue;
                    }
                    let summary = SessionSummary {
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
                        connection_state: Some(RemoteSessionConnectionState::Disconnected),
                        runtime_state: session_info.runtime_state,
                        workspace: None,
                    };
                    push_group_session(groups, format!("remote::{}", peer_label), summary);
                }
            }
        }
    }
}

impl From<crate::session::projection::SessionGroup> for SessionGroup {
    fn from(group: crate::session::projection::SessionGroup) -> Self {
        Self {
            cwd: group.cwd,
            sessions: group.sessions.into_iter().map(Into::into).collect(),
            latest_activity: group.latest_activity.and_then(|t| t.format(&Rfc3339).ok()),
            total_count: group.total_count.map(|v| v as u64),
            next_cursor: group.next_cursor,
        }
    }
}

/// Read a deterministic, deduplicated QueryMT opt-in; absent IDs keep listing local-only.
#[cfg(feature = "remote")]
fn acp_remote_node_ids(meta: Option<&Meta>) -> Vec<String> {
    let mut ids: Vec<String> = meta
        .and_then(|meta| meta.get("remoteNodeIds"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Reject oversized pages and continuations that cannot make forward progress.
#[cfg(feature = "remote")]
fn validate_acp_remote_page(
    response: &crate::agent::remote::ListRemoteSessionsResponse,
    offset: u32,
    limit: u32,
) -> std::result::Result<(), AcpSessionListError> {
    if response.sessions.len() > limit as usize {
        return Err(anyhow!("Remote session page exceeds requested limit").into());
    }
    if let Some(next) = response.next_offset {
        let expected = offset.checked_add(response.sessions.len() as u32);
        if response.sessions.is_empty() || next <= offset || Some(next) != expected {
            return Err(anyhow!("Remote session page has a non-progressing continuation").into());
        }
    }
    Ok(())
}

/// Decode an ACP cursor whose offset is in the owner's unfiltered session list.
#[cfg(feature = "remote")]
fn parse_acp_remote_cursor(
    cursor: &str,
) -> std::result::Result<AcpRemoteCursor, AcpSessionListError> {
    let mut parts = cursor.split(':');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some("remote"), Some(node), Some(offset), source, None) => {
            let source = match source {
                None => None,
                Some("live") => Some(AcpRemoteSource::Live),
                Some("bookmarks") => Some(AcpRemoteSource::Bookmarks),
                _ => return Err(AcpSessionListError::InvalidCursor),
            };
            Ok(AcpRemoteCursor {
                node: node
                    .parse()
                    .map_err(|_| AcpSessionListError::InvalidCursor)?,
                offset: offset
                    .parse()
                    .map_err(|_| AcpSessionListError::InvalidCursor)?,
                source,
            })
        }
        _ => Err(AcpSessionListError::InvalidCursor),
    }
}

fn insert_acp_lifecycle_meta(meta: &mut Meta, lifecycle: &SessionLifecycle) {
    let namespace = meta
        .entry("querymt")
        .or_insert_with(|| serde_json::json!({}));
    if !namespace.is_object() {
        *namespace = serde_json::json!({});
    }
    if let Some(namespace) = namespace.as_object_mut() {
        namespace.insert(
            "lifecycle".into(),
            serde_json::to_value(lifecycle).expect("serialize lifecycle metadata"),
        );
    }
}

fn insert_acp_pin_meta(meta: &mut Meta, pin: &crate::session::store::SessionPin) {
    let namespace = meta
        .entry("querymt")
        .or_insert_with(|| serde_json::json!({}));
    if !namespace.is_object() {
        *namespace = serde_json::json!({});
    }
    if let Some(namespace) = namespace.as_object_mut() {
        namespace.insert(
            "pin".into(),
            serde_json::json!({
                "pinned": pin.pinned,
                "pinned_at": pin.pinned_at,
            }),
        );
    }
}

fn acp_session_scope_from_meta(meta: Option<&Meta>) -> SessionScope {
    let Some(meta) = meta else {
        return SessionScope::All;
    };

    let value = meta
        .get("session_scope")
        .or_else(|| meta.get("sessionScope"));
    match value {
        Some(serde_json::Value::String(scope)) => SessionScope::from_option(Some(scope.clone())),
        _ => SessionScope::All,
    }
}

/// `_meta.querymt.parentSessionId` (also accepted at the top level of the meta
/// for symmetry with `sessionScope`), if a non-empty string.
fn acp_parent_session_id_from_meta(meta: Option<&Meta>) -> Option<String> {
    let meta = meta?;
    meta.get(crate::acp::shared::QUERYMT_META_NAMESPACE)
        .and_then(|querymt| querymt.get("parentSessionId"))
        .or_else(|| meta.get("parentSessionId"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn session_list_item_to_acp_info(item: SessionListItem) -> SessionInfo {
    let mut info = SessionInfo::new(
        SessionId::from(item.session_id),
        item.cwd.map(PathBuf::from).unwrap_or_default(),
    );
    info.title = item.name.or(item.title);
    info.updated_at = item
        .updated_at
        .and_then(|updated_at| updated_at.format(&Rfc3339).ok());

    let mut meta = Meta::new();
    if let Some(parent_session_id) = item.parent_session_id {
        meta.insert(
            "parentSessionId".to_string(),
            serde_json::Value::String(parent_session_id),
        );
    }
    if let Some(fork_origin) = item.fork_origin {
        meta.insert(
            "forkOrigin".to_string(),
            serde_json::Value::String(fork_origin),
        );
    }
    if let Some(session_kind) = item.session_kind {
        meta.insert(
            "sessionKind".to_string(),
            serde_json::Value::String(session_kind),
        );
    }
    meta.insert(
        "hasChildren".to_string(),
        serde_json::Value::Bool(item.has_children),
    );
    meta.insert(
        "forkCount".to_string(),
        serde_json::Value::from(item.fork_count),
    );
    if let Some(workspace) = &item.workspace {
        let mut querymt = serde_json::Map::new();
        querymt.insert("workspace".to_string(), workspace.to_meta());
        meta.insert(
            crate::acp::shared::QUERYMT_META_NAMESPACE.to_string(),
            querymt.into(),
        );
    }
    info.meta = Some(meta);
    info
}

impl From<SessionListItem> for SessionSummary {
    fn from(value: SessionListItem) -> Self {
        Self {
            session_id: value.session_id,
            name: value.name,
            cwd: value.cwd,
            title: value.title,
            created_at: value.created_at.and_then(|t| t.format(&Rfc3339).ok()),
            updated_at: value.updated_at.and_then(|t| t.format(&Rfc3339).ok()),
            parent_session_id: value.parent_session_id,
            fork_origin: value.fork_origin,
            session_kind: value.session_kind,
            has_children: value.has_children,
            fork_count: value.fork_count as u64,
            node: None,
            node_id: None,
            attached: None,
            connection_state: None,
            runtime_state: None,
            workspace: value.workspace,
        }
    }
}

fn normalize_group_cwd(cwd: String) -> String {
    let path = std::path::Path::new(&cwd);
    if path.is_absolute() {
        path.components().collect::<PathBuf>().display().to_string()
    } else {
        cwd
    }
}

#[cfg(feature = "remote")]
fn push_group_session(groups: &mut Vec<SessionGroup>, group_cwd: String, summary: SessionSummary) {
    if let Some(existing) = groups
        .iter_mut()
        .find(|g| g.cwd.as_deref() == Some(group_cwd.as_str()))
    {
        if !existing
            .sessions
            .iter()
            .any(|session| session.session_id == summary.session_id)
        {
            existing.sessions.push(summary);
        }
        return;
    }

    groups.push(SessionGroup {
        cwd: Some(group_cwd),
        sessions: vec![summary],
        latest_activity: None,
        total_count: None,
        next_cursor: None,
    });
}

#[cfg(all(test, feature = "remote"))]
mod remote_page_tests {
    use super::*;
    use crate::agent::remote::{ListRemoteSessionsResponse, RemoteSessionSnapshot};

    fn response(count: usize, next_offset: Option<u32>) -> ListRemoteSessionsResponse {
        ListRemoteSessionsResponse {
            sessions: (0..count)
                .map(|index| RemoteSessionSnapshot {
                    session_id: format!("remote-{index}"),
                    actor_id: 0,
                    cwd: None,
                    created_at: 0,
                    updated_at: None,
                    title: None,
                    profile_id: None,
                    profile_label: None,
                    peer_label: "owner".into(),
                    runtime_state: None,
                    parent_session_id: None,
                    fork_origin: None,
                })
                .collect(),
            next_offset,
            total_count: 100,
        }
    }

    #[test]
    fn remote_cursor_offsets_are_scoped_to_their_list_source() {
        let live = parse_acp_remote_cursor("remote:2:30:live").expect("live cursor");
        assert_eq!(live.node, 2);
        assert_eq!(live.offset_for(AcpRemoteSource::Live), 30);
        assert_eq!(live.offset_for(AcpRemoteSource::Bookmarks), 0);
        let bookmarks = parse_acp_remote_cursor("remote:2:10:bookmarks").expect("bookmark cursor");
        assert_eq!(bookmarks.offset_for(AcpRemoteSource::Live), 0);
        assert_eq!(bookmarks.offset_for(AcpRemoteSource::Bookmarks), 10);
        let legacy = parse_acp_remote_cursor("remote:2:10").expect("legacy cursor");
        assert_eq!(legacy.offset_for(AcpRemoteSource::Live), 0);
        assert_eq!(legacy.offset_for(AcpRemoteSource::Bookmarks), 0);
        for invalid in [
            "remote:2:10:unknown",
            "remote:2:10:live:extra",
            "remote:2:4294967296:live",
        ] {
            assert!(parse_acp_remote_cursor(invalid).is_err());
        }
    }

    #[test]
    fn owner_pages_are_bounded_and_continuations_are_strictly_monotonic() {
        assert!(validate_acp_remote_page(&response(10, Some(20)), 10, 10).is_ok());
        assert!(validate_acp_remote_page(&response(5, None), 20, 10).is_ok());
        assert!(validate_acp_remote_page(&response(0, None), 100, 10).is_ok());
        for (count, offset, next) in [
            (11, 0, Some(11)),
            (10, 10, Some(10)),
            (10, 20, Some(10)),
            (0, 0, Some(1)),
            (10, 0, Some(1000)),
            (1, u32::MAX, Some(0)),
        ] {
            assert!(validate_acp_remote_page(&response(count, next), offset, 10).is_err());
        }
    }

    fn summary(session_id: &str, node_id: &str) -> SessionSummary {
        SessionSummary {
            session_id: session_id.to_string(),
            name: None,
            cwd: None,
            title: None,
            created_at: None,
            updated_at: None,
            parent_session_id: None,
            fork_origin: None,
            session_kind: None,
            has_children: false,
            fork_count: 0,
            node: None,
            node_id: Some(node_id.to_string()),
            attached: None,
            connection_state: None,
            runtime_state: None,
            workspace: None,
        }
    }

    fn lifecycle(session_id: &str, resolved: bool) -> SessionLifecycle {
        SessionLifecycle {
            session_id: session_id.to_string(),
            resolved,
            archived: false,
            resolved_at: resolved.then(|| "2026-01-01T00:00:00Z".to_string()),
            archived_at: None,
        }
    }

    #[tokio::test]
    async fn lifecycle_enrichment_discovers_each_node_once_and_fetches_session_states() {
        use std::sync::{Arc, Mutex};

        let find_calls = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let fetch_calls = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let summaries = vec![
            summary("a-open", "node-a"),
            summary("a-resolved", "node-a"),
            summary("b-open", "node-b"),
        ];
        let lifecycles = load_remote_lifecycles_for_summaries(
            &summaries,
            {
                let calls = Arc::clone(&find_calls);
                move |node_id: String| {
                    let calls = Arc::clone(&calls);
                    async move {
                        *calls.lock().unwrap().entry(node_id.clone()).or_default() += 1;
                        Some(node_id)
                    }
                }
            },
            {
                let calls = Arc::clone(&fetch_calls);
                move |_manager: String, session_id: String| {
                    let calls = Arc::clone(&calls);
                    async move {
                        *calls.lock().unwrap().entry(session_id.clone()).or_default() += 1;
                        Some(lifecycle(&session_id, session_id == "a-resolved"))
                    }
                }
            },
        )
        .await;

        assert_eq!(
            find_calls.lock().unwrap().clone(),
            HashMap::from([("node-a".to_string(), 1), ("node-b".to_string(), 1)])
        );
        assert_eq!(
            fetch_calls.lock().unwrap().clone(),
            HashMap::from([
                ("a-open".to_string(), 1),
                ("a-resolved".to_string(), 1),
                ("b-open".to_string(), 1),
            ])
        );
        assert_eq!(
            lifecycles.get("a-open"),
            Some(&Some(lifecycle("a-open", false)))
        );
        assert_eq!(
            lifecycles.get("a-resolved"),
            Some(&Some(lifecycle("a-resolved", true)))
        );
        assert_eq!(
            lifecycles.get("b-open"),
            Some(&Some(lifecycle("b-open", false)))
        );
    }

    #[tokio::test]
    async fn lifecycle_enrichment_processes_rows_beyond_node_and_session_windows() {
        use std::sync::{Arc, Mutex};

        let find_calls = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let fetch_calls = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let mut summaries = (0..8)
            .map(|node| summary(&format!("node-session-{node}"), &format!("node-{node:02}")))
            .collect::<Vec<_>>();
        summaries
            .extend((0..101).map(|session| summary(&format!("session-{session:03}"), "node-08")));

        let lifecycles = load_remote_lifecycles_for_summaries(
            &summaries,
            {
                let calls = Arc::clone(&find_calls);
                move |node_id: String| {
                    let calls = Arc::clone(&calls);
                    async move {
                        *calls.lock().unwrap().entry(node_id).or_default() += 1;
                        Some("healthy-owner".to_string())
                    }
                }
            },
            {
                let calls = Arc::clone(&fetch_calls);
                move |manager: String, session_id: String| {
                    let calls = Arc::clone(&calls);
                    async move {
                        assert_eq!(manager, "healthy-owner");
                        *calls.lock().unwrap().entry(session_id.clone()).or_default() += 1;
                        Some(lifecycle(&session_id, session_id == "session-100"))
                    }
                }
            },
        )
        .await;

        let find_calls = find_calls.lock().unwrap();
        assert_eq!(find_calls.len(), 9);
        assert!(find_calls.values().all(|count| *count == 1));
        let fetch_calls = fetch_calls.lock().unwrap();
        assert_eq!(fetch_calls.len(), 109);
        assert!(fetch_calls.values().all(|count| *count == 1));
        for summary in &summaries {
            let resolved = summary.session_id == "session-100";
            assert_eq!(
                lifecycles.get(&summary.session_id),
                Some(&Some(lifecycle(&summary.session_id, resolved)))
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lifecycle_enrichment_fetches_100_healthy_sessions_within_shared_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let find_calls = Arc::new(AtomicUsize::new(0));
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let summaries = (0..100)
            .map(|index| summary(&format!("session-{index:03}"), "healthy-owner"))
            .collect::<Vec<_>>();
        let lifecycles = load_remote_lifecycles_for_summaries(
            &summaries,
            {
                let find_calls = Arc::clone(&find_calls);
                move |_node_id: String| {
                    let find_calls = Arc::clone(&find_calls);
                    async move {
                        find_calls.fetch_add(1, Ordering::SeqCst);
                        Some("healthy-owner".to_string())
                    }
                }
            },
            {
                let fetch_calls = Arc::clone(&fetch_calls);
                let in_flight = Arc::clone(&in_flight);
                let max_in_flight = Arc::clone(&max_in_flight);
                move |manager: String, session_id: String| {
                    let fetch_calls = Arc::clone(&fetch_calls);
                    let in_flight = Arc::clone(&in_flight);
                    let max_in_flight = Arc::clone(&max_in_flight);
                    async move {
                        assert_eq!(manager, "healthy-owner");
                        fetch_calls.fetch_add(1, Ordering::SeqCst);
                        let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                        max_in_flight.fetch_max(active, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(60)).await;
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                        let index = session_id
                            .strip_prefix("session-")
                            .unwrap()
                            .parse::<usize>()
                            .unwrap();
                        Some(lifecycle(&session_id, index % 2 == 0))
                    }
                }
            },
        )
        .await;

        let completed_count = lifecycles.values().filter(|state| state.is_some()).count();
        assert_eq!(
            completed_count,
            summaries.len(),
            "all healthy lifecycle lookups should fit within the shared node deadline"
        );
        for (index, summary) in summaries.iter().enumerate() {
            assert_eq!(
                lifecycles.get(&summary.session_id),
                Some(&Some(lifecycle(&summary.session_id, index % 2 == 0)))
            );
        }
        assert_eq!(find_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fetch_calls.load(Ordering::SeqCst), summaries.len());
        assert!(max_in_flight.load(Ordering::SeqCst) > 1);
        assert!(max_in_flight.load(Ordering::SeqCst) <= REMOTE_LIFECYCLE_CONCURRENCY);
    }

    #[tokio::test(start_paused = true)]
    async fn lifecycle_enrichment_keeps_completed_states_and_bounds_offline_or_slow_nodes() {
        use std::sync::{Arc, Mutex};

        let fetch_calls = Arc::new(Mutex::new(Vec::new()));
        let summaries = vec![
            summary("offline-a", "node-offline"),
            summary("offline-b", "node-offline"),
            summary("known", "node-slow"),
            summary("slow", "node-slow"),
            summary("after-timeout", "node-slow"),
        ];
        let lifecycles = load_remote_lifecycles_for_summaries(
            &summaries,
            |node_id: String| async move {
                if node_id == "node-offline" {
                    None
                } else {
                    Some(node_id)
                }
            },
            {
                let calls = Arc::clone(&fetch_calls);
                move |_manager: String, session_id: String| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.lock().unwrap().push(session_id.clone());
                        if session_id == "slow" {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                        Some(lifecycle(&session_id, true))
                    }
                }
            },
        )
        .await;

        assert_eq!(lifecycles.get("offline-a"), Some(&None));
        assert_eq!(lifecycles.get("offline-b"), Some(&None));
        assert_eq!(
            lifecycles.get("known"),
            Some(&Some(lifecycle("known", true)))
        );
        assert_eq!(lifecycles.get("slow"), Some(&None));
        assert_eq!(
            lifecycles.get("after-timeout"),
            Some(&Some(lifecycle("after-timeout", true)))
        );
        let fetch_calls = fetch_calls.lock().unwrap();
        assert_eq!(fetch_calls.len(), 3);
        assert_eq!(
            fetch_calls
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from([
                "known".to_string(),
                "slow".to_string(),
                "after-timeout".to_string(),
            ])
        );
    }
}
