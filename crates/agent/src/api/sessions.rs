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
        #[cfg(feature = "remote")]
        let remote_nodes = acp_remote_node_ids(request.meta.as_ref());
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
                cursor,
            )
            .await;
        }

        let mut page = Self::list_for_acp_from_view_store(view_store.clone(), request).await?;
        Self::hydrate_acp_session_meta(agent, view_store, &mut page.sessions).await?;
        #[cfg(feature = "remote")]
        if !remote_nodes.is_empty() && page.next_cursor.is_none() {
            let remote_page = Self::list_acp_remote_page(
                agent,
                &remote_nodes,
                remote_cwd.as_deref(),
                remote_scope,
                (0, 0),
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
        cursor: (usize, usize),
    ) -> std::result::Result<AcpSessionListPage, AcpSessionListError> {
        let node_id = nodes
            .get(cursor.0)
            .ok_or(AcpSessionListError::InvalidCursor)?;
        let offset = u32::try_from(cursor.1).map_err(|_| AcpSessionListError::InvalidCursor)?;
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

        // Bound lookup plus listing together; a small ACP request must not
        // enumerate every peer or repeatedly traverse an owner's history.
        let live_page = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let manager = agent.find_node_manager(node_id).await?;
            agent
                .list_remote_sessions(&manager, Some(offset), Some(limit))
                .await
        })
        .await;
        let (entries, next_offset, live) = match live_page {
            Ok(Ok(response)) => {
                validate_acp_remote_page(&response, offset, limit)?;
                (response.sessions, response.next_offset, true)
            }
            error => {
                log::debug!("Unable to list sessions from peer {}: {:?}", node_id, error);
                let peer_bookmarks: Vec<_> = bookmarks
                    .iter()
                    .filter(|bookmark| bookmark.node_id == *node_id)
                    .collect();
                let entries: Vec<_> = peer_bookmarks
                    .iter()
                    .skip(cursor.1)
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
                let end = cursor.1.saturating_add(entries.len());
                let next = if end < peer_bookmarks.len() {
                    Some(u32::try_from(end).map_err(|_| AcpSessionListError::InvalidCursor)?)
                } else {
                    None
                };
                (entries, next, false)
            }
        };
        let next_cursor = next_offset
            .map(|offset| format!("remote:{}:{offset}", cursor.0))
            .or_else(|| (cursor.0 + 1 < nodes.len()).then(|| format!("remote:{}:0", cursor.0 + 1)));
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
                SessionScope::Forks => entry.fork_origin.as_deref() == Some("user"),
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
        let cursor = AcpSessionCursor::parse(request.cursor.as_deref())?;
        let requested_cwd = request
            .cwd
            .map(|cwd| normalize_group_cwd(cwd.display().to_string()));
        let session_scope = acp_session_scope_from_meta(request.meta.as_ref());
        // ACP workspace requests load incrementally; global discovery remains a larger flat page.
        let limit = if requested_cwd.is_some() { 10 } else { 100 };

        let (sessions, next_cursor, total_count) = view_store
            .list_session_items(
                requested_cwd,
                cursor.map(AcpSessionCursor::into_string),
                limit,
                session_scope,
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
        // Remove the durable remote bookmark first: if that fails, return
        // early with the session row intact instead of deleting the row and
        // leaving a dangling bookmark behind.
        self.session_store()
            .remove_remote_session_bookmark(&session_id)
            .await?;
        self.session_store().delete_session(&session_id).await?;
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
) -> std::result::Result<(usize, usize), AcpSessionListError> {
    let mut parts = cursor.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("remote"), Some(node), Some(offset), None) => {
            let node = node
                .parse()
                .map_err(|_| AcpSessionListError::InvalidCursor)?;
            let offset = offset
                .parse()
                .map_err(|_| AcpSessionListError::InvalidCursor)?;
            Ok((node, offset))
        }
        _ => Err(AcpSessionListError::InvalidCursor),
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
}
