use super::session::AgentSession;
use crate::acp::protocol::{
    ListSessionsRequest as AcpListSessionsRequest, ListSessionsResponse as AcpListSessionsResponse,
    LoadSessionRequest, Meta, NewSessionRequest, SessionId, SessionInfo,
};
use crate::agent::LocalAgentHandle;
use crate::agent::messages::SessionRuntimeStatus;
use crate::session::load_snapshot::{SessionLoadSnapshot, load_session_snapshot};
use crate::session::projection::{SessionListItem, SessionScope, ViewStore};
use crate::session::store::SessionStore;
use anyhow::{Result, anyhow};
use querymt::chat::FinishReason;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use typeshare::typeshare;

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
    pub runtime_state: Option<String>,
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
    pub total_count: u64,
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
                    _ => cwd,
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
        Self::list_for_acp_with_runtime(&self.agent, self.view_store()?, request).await
    }

    pub(crate) async fn list_for_acp_with_runtime(
        agent: &LocalAgentHandle,
        view_store: Arc<dyn ViewStore>,
        request: AcpListSessionsRequest,
    ) -> Result<AcpSessionListPage> {
        let mut page = Self::list_for_acp_from_view_store(view_store.clone(), request).await?;
        Self::hydrate_acp_session_meta(agent, view_store, &mut page.sessions).await?;
        Ok(page)
    }

    pub(crate) async fn list_for_acp_from_view_store(
        view_store: Arc<dyn ViewStore>,
        request: AcpListSessionsRequest,
    ) -> Result<AcpSessionListPage> {
        let requested_cwd = request.cwd.map(|cwd| cwd.display().to_string());
        let limit = 100usize;

        let (sessions, next_cursor, total_count) = view_store
            .list_session_items(requested_cwd, request.cursor, limit, SessionScope::All)
            .await?;

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

        let persisted_stats = view_store.get_session_list_meta_stats(&session_ids).await?;
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
                .copied()
                .unwrap_or(SessionRuntimeStatus::Idle);
            let has_errors = stats.last_finish_reason == Some(FinishReason::Error);
            let meta = SessionMeta {
                message_count: stats.message_count,
                user_message_count: stats.user_message_count,
                has_errors,
                runtime_status,
            };
            info.meta = Some(meta.to_acp_meta());
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
        Ok(AgentSession::new(self.agent.clone(), session_id))
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
        Ok(AgentLoadedSession {
            session: AgentSession::new(self.agent.clone(), session_id),
            snapshot,
        })
    }

    pub async fn delete(&self, session_id: impl AsRef<str>) -> Result<()> {
        let session_id = session_id.as_ref().to_string();
        self.session_store().delete_session(&session_id).await?;
        let mut registry = self.agent.registry.lock().await;
        #[cfg(feature = "remote")]
        {
            registry.detach_remote_session(&session_id).await;
        }
        #[cfg(not(feature = "remote"))]
        {
            registry.remove(&session_id);
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
                        runtime_state: None,
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
                        runtime_state: session_info.runtime_state,
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

fn session_list_item_to_acp_info(item: SessionListItem) -> SessionInfo {
    let mut info = SessionInfo::new(
        SessionId::from(item.session_id),
        item.cwd.map(PathBuf::from).unwrap_or_default(),
    );
    info.title = item.name.or(item.title);
    info.updated_at = item
        .updated_at
        .and_then(|updated_at| updated_at.format(&Rfc3339).ok());
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
            runtime_state: None,
        }
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
