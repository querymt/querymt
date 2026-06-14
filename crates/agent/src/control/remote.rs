use crate::LocalAgentHandle;
use agent_client_protocol::Error;
use serde::{Deserialize, Serialize};
use typeshare::typeshare;

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteNodeInfo {
    pub id: String,
    pub label: String,
    pub capabilities: Vec<String>,
    pub active_sessions: u32,
    pub transport: String,
    pub last_seen_at: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionInfo {
    pub id: String,
    pub node_id: String,
    pub node_label: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub updated_at: Option<String>,
    pub profile_id: Option<String>,
    pub model_id: Option<String>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionListInfo {
    pub node_id: String,
    pub sessions: Vec<RemoteSessionInfo>,
    pub next_offset: Option<u32>,
    pub total_count: u32,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionsRequest {
    pub node_id: String,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRemoteSessionRequest {
    pub node_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_attach")]
    pub attach: bool,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachRemoteSessionRequest {
    pub node_id: String,
    pub session_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DismissRemoteSessionRequest {
    pub session_id: String,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionAttachInfo {
    pub session_id: String,
    pub node_id: String,
    pub attached: bool,
    #[typeshare(serialized_as = "Array<any>")]
    pub config_options: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[typeshare(serialized_as = "any")]
    pub snapshot: Option<serde_json::Value>,
}

#[typeshare]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSessionDismissInfo {
    pub success: bool,
    pub session_id: String,
}

fn default_attach() -> bool {
    true
}

#[cfg(feature = "remote")]
fn remote_snapshot_to_info(
    node_id: &str,
    session: crate::agent::remote::RemoteSessionSnapshot,
) -> RemoteSessionInfo {
    let updated_at = time::OffsetDateTime::from_unix_timestamp(session.created_at)
        .ok()
        .and_then(|ts| {
            ts.format(&time::format_description::well_known::Rfc3339)
                .ok()
        });
    RemoteSessionInfo {
        id: session.session_id,
        node_id: node_id.to_string(),
        node_label: Some(session.peer_label),
        title: session.title,
        cwd: session.cwd,
        updated_at,
        profile_id: None,
        model_id: None,
    }
}

pub async fn list_remote_nodes(agent: &LocalAgentHandle) -> Vec<RemoteNodeInfo> {
    #[cfg(feature = "remote")]
    {
        return agent
            .list_remote_nodes()
            .await
            .into_iter()
            .map(|node| RemoteNodeInfo {
                id: node.node_id.to_string(),
                label: node.hostname,
                capabilities: node.capabilities,
                active_sessions: crate::agent::utils::u32_from_usize(
                    node.active_sessions,
                    "active_sessions",
                    None,
                ),
                transport: "unknown".to_string(),
                last_seen_at: None,
            })
            .collect();
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        Vec::new()
    }
}

pub async fn list_remote_sessions(
    agent: &LocalAgentHandle,
    request: RemoteSessionsRequest,
) -> Result<RemoteSessionListInfo, Error> {
    #[cfg(feature = "remote")]
    {
        let nm_ref = agent.find_node_manager(&request.node_id).await?;
        let response = agent
            .list_remote_sessions(&nm_ref, request.offset, request.limit)
            .await?;
        Ok(RemoteSessionListInfo {
            node_id: request.node_id.clone(),
            sessions: response
                .sessions
                .into_iter()
                .map(|session| remote_snapshot_to_info(&request.node_id, session))
                .collect(),
            next_offset: response.next_offset,
            total_count: response.total_count,
        })
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        let _ = request;
        Err(Error::method_not_found())
    }
}

pub async fn create_remote_session(
    agent: &LocalAgentHandle,
    request: CreateRemoteSessionRequest,
) -> Result<RemoteSessionAttachInfo, Error> {
    #[cfg(feature = "remote")]
    {
        let nm_ref = agent.find_node_manager(&request.node_id).await?;
        let response = agent
            .create_remote_session(&nm_ref, request.cwd.clone())
            .await?;

        if !request.attach {
            return Ok(RemoteSessionAttachInfo {
                session_id: response.session_id,
                node_id: request.node_id,
                attached: false,
                config_options: Vec::new(),
                snapshot: None,
            });
        }

        let snapshot = agent
            .attach_remote_session_for_ext(
                &request.node_id,
                &response.session_id,
                Some(response.handoff),
            )
            .await?;

        Ok(RemoteSessionAttachInfo {
            session_id: response.session_id,
            node_id: request.node_id,
            attached: true,
            config_options: Vec::new(),
            snapshot: Some(snapshot),
        })
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        let _ = request;
        Err(Error::method_not_found())
    }
}

pub async fn attach_remote_session(
    agent: &LocalAgentHandle,
    request: AttachRemoteSessionRequest,
) -> Result<RemoteSessionAttachInfo, Error> {
    #[cfg(feature = "remote")]
    {
        let snapshot = agent
            .attach_remote_session_for_ext(&request.node_id, &request.session_id, None)
            .await?;
        Ok(RemoteSessionAttachInfo {
            session_id: request.session_id,
            node_id: request.node_id,
            attached: true,
            config_options: Vec::new(),
            snapshot: Some(snapshot),
        })
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        let _ = request;
        Err(Error::method_not_found())
    }
}

pub async fn dismiss_remote_session(
    agent: &LocalAgentHandle,
    request: DismissRemoteSessionRequest,
) -> Result<RemoteSessionDismissInfo, Error> {
    #[cfg(feature = "remote")]
    {
        {
            let mut registry = agent.registry.lock().await;
            registry.detach_remote_session(&request.session_id).await;
        }

        agent
            .config
            .provider
            .history_store()
            .remove_remote_session_bookmark(&request.session_id)
            .await
            .map_err(|e| {
                Error::internal_error().data(serde_json::json!({"error": e.to_string()}))
            })?;

        Ok(RemoteSessionDismissInfo {
            success: true,
            session_id: request.session_id,
        })
    }

    #[cfg(not(feature = "remote"))]
    {
        let _ = agent;
        let _ = request;
        Err(Error::method_not_found())
    }
}
