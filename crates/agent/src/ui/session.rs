//! Session management and routing mode logic.
//!
//! Handles session creation, agent lookup, routing modes (single/broadcast),
//! and session-related state management.

use super::ServerState;
use super::messages::{RoutingMode, UiAgentInfo, UiPromptBlock, UiServerMessage};
use crate::agent::AgentHandle;
use crate::index::{normalize_cwd, resolve_workspace_root};
use crate::send_agent::SendAgent;
use agent_client_protocol::{ContentBlock, NewSessionRequest, PromptRequest};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub const PRIMARY_AGENT_ID: &str = "primary";

/// Ensure sessions exist for the current routing mode.
pub async fn ensure_sessions_for_mode(
    state: &ServerState,
    conn_id: &str,
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
    request_id: Option<&str>,
) -> Result<(), String> {
    let mode = current_mode(state, conn_id).await?;
    match mode {
        RoutingMode::Single => {
            let agent_id = current_active_agent(state, conn_id).await?;
            ensure_session(state, conn_id, &agent_id, cwd, tx, request_id).await?;
        }
        RoutingMode::Broadcast => {
            let agent_ids = list_agent_ids(state);
            for (i, agent_id) in agent_ids.iter().enumerate() {
                // Only pass request_id to the first agent in broadcast mode
                let req_id = if i == 0 { request_id } else { None };
                ensure_session(state, conn_id, agent_id, cwd, tx, req_id).await?;
            }
        }
    }
    Ok(())
}

/// Send a prompt to agents based on the current routing mode.
pub async fn prompt_for_mode(
    state: &ServerState,
    conn_id: &str,
    prompt: &[UiPromptBlock],
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    let mode = current_mode(state, conn_id).await?;
    match mode {
        RoutingMode::Single => {
            let agent_id = current_active_agent(state, conn_id).await?;
            let session_id = ensure_session(state, conn_id, &agent_id, cwd, tx, None).await?;
            let agent = agent_for_id(state, &agent_id)
                .ok_or_else(|| format!("Unknown agent: {}", agent_id))?;
            let session_cwd = session_cwd_for(state, &session_id).await.or(cwd.cloned());
            let session_ref = {
                let registry = state.agent.registry.lock().await;
                registry.get(&session_id).cloned()
            };
            let prompt_blocks = super::mentions::build_prompt_blocks(
                &state.workspace_manager,
                session_cwd.as_ref(),
                prompt,
                session_ref.as_ref(),
            )
            .await;
            send_prompt(agent, session_id, prompt_blocks).await?;
        }
        RoutingMode::Broadcast => {
            let agent_ids = list_agent_ids(state);
            for agent_id in agent_ids {
                let session_id = ensure_session(state, conn_id, &agent_id, cwd, tx, None).await?;
                let agent = agent_for_id(state, &agent_id)
                    .ok_or_else(|| format!("Unknown agent: {}", agent_id))?;
                let session_cwd = session_cwd_for(state, &session_id).await.or(cwd.cloned());
                let session_ref = {
                    let registry = state.agent.registry.lock().await;
                    registry.get(&session_id).cloned()
                };
                let prompt_blocks = super::mentions::build_prompt_blocks(
                    &state.workspace_manager,
                    session_cwd.as_ref(),
                    prompt,
                    session_ref.as_ref(),
                )
                .await;
                send_prompt(agent, session_id, prompt_blocks).await?;
            }
        }
    }
    Ok(())
}

/// Send a prompt to a specific agent session.
async fn send_prompt(
    agent: Arc<dyn SendAgent>,
    session_id: String,
    prompt: Vec<ContentBlock>,
) -> Result<(), String> {
    let request = PromptRequest::new(session_id, prompt);
    agent
        .prompt(request)
        .await
        .map_err(|err| err.message)
        .map(|_| ())
}

/// Ensure a session exists for the given agent, creating one if needed.
pub async fn ensure_session(
    state: &ServerState,
    conn_id: &str,
    agent_id: &str,
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
    request_id: Option<&str>,
) -> Result<String, String> {
    let existing = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(agent_id).cloned())
    };
    if let Some(session_id) = existing {
        return Ok(session_id);
    }

    let agent =
        agent_for_id(state, agent_id).ok_or_else(|| format!("Unknown agent: {}", agent_id))?;

    // Use empty PathBuf as sentinel for "no cwd" to work with ACP protocol
    let cwd_for_request = cwd.cloned().unwrap_or_else(PathBuf::new);
    let response = agent
        .new_session(NewSessionRequest::new(cwd_for_request))
        .await
        .map_err(|err| err.message)?;
    let session_id = response.session_id.to_string();

    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.sessions
                .insert(agent_id.to_string(), session_id.clone());
        }
    }

    if let Some(cwd_path) = cwd.cloned() {
        let mut cwds = state.session_cwds.lock().await;
        cwds.insert(session_id.clone(), cwd_path);
    }

    {
        let mut agents = state.session_agents.lock().await;
        agents.insert(session_id.clone(), agent_id.to_string());
    }

    // Auto-subscribe the connection to this session
    {
        let mut connections = state.connections.lock().await;
        if let Some(conn) = connections.get_mut(conn_id) {
            conn.subscribed_sessions.insert(session_id.clone());
        }
    }

    let _ = super::connection::send_message(
        tx,
        UiServerMessage::SessionCreated {
            agent_id: agent_id.to_string(),
            session_id: session_id.clone(),
            request_id: request_id.map(|s| s.to_string()),
        },
    )
    .await;

    // Replay stored events for the new session (includes ProviderChanged)
    // No child sessions for a new session
    if let Ok(audit) = state.view_store.get_audit_view(&session_id, false).await {
        let cursor_seq = audit
            .events
            .iter()
            .map(|event| event.seq)
            .max()
            .unwrap_or(0);

        {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.session_cursors.insert(session_id.clone(), cursor_seq);
            }
        }

        let _ = super::connection::send_message(
            tx,
            UiServerMessage::SessionEvents {
                session_id: session_id.clone(),
                agent_id: agent_id.to_string(),
                events: audit.events,
                cursor_seq,
            },
        )
        .await;
    }

    if let Some(cwd_path) = cwd.cloned() {
        let root = resolve_workspace_root(&cwd_path);
        let manager = state.workspace_manager.clone();
        let session_id_clone = session_id.clone();
        let tx_clone = tx.clone();
        let state_clone = state.clone();
        let conn_id_clone = conn_id.to_string();

        let _ = super::connection::send_message(
            tx,
            UiServerMessage::WorkspaceIndexStatus {
                session_id: session_id.clone(),
                status: "building".to_string(),
                message: None,
            },
        )
        .await;

        tokio::spawn(async move {
            let status = match manager
                .ask(crate::index::GetOrCreate { root: root.clone() })
                .await
            {
                Ok(_) => {
                    // Subscribe to file index updates for this workspace
                    super::connection::subscribe_to_file_index(
                        state_clone,
                        conn_id_clone,
                        tx_clone.clone(),
                        root,
                    )
                    .await;

                    UiServerMessage::WorkspaceIndexStatus {
                        session_id: session_id_clone,
                        status: "ready".to_string(),
                        message: None,
                    }
                }
                Err(err) => UiServerMessage::WorkspaceIndexStatus {
                    session_id: session_id_clone,
                    status: "error".to_string(),
                    message: Some(err.to_string()),
                },
            };

            let _ = super::connection::send_message(&tx_clone, status).await;
        });
    }

    super::connection::send_state(state, conn_id, tx).await;

    Ok(session_id)
}

/// Get an agent instance by ID.
pub fn agent_for_id(state: &ServerState, agent_id: &str) -> Option<Arc<dyn SendAgent>> {
    if agent_id == PRIMARY_AGENT_ID {
        return Some(state.agent.clone());
    }
    let registry = state.agent.agent_registry();
    registry.get_agent_instance(agent_id)
}

/// Get the current active agent ID for a connection.
pub async fn current_active_agent(state: &ServerState, conn_id: &str) -> Result<String, String> {
    let connections = state.connections.lock().await;
    connections
        .get(conn_id)
        .map(|conn| conn.active_agent_id.clone())
        .ok_or_else(|| "Connection state missing".to_string())
}

/// Get the current routing mode for a connection.
pub async fn current_mode(state: &ServerState, conn_id: &str) -> Result<RoutingMode, String> {
    let connections = state.connections.lock().await;
    connections
        .get(conn_id)
        .map(|conn| conn.routing_mode)
        .ok_or_else(|| "Connection state missing".to_string())
}

/// List all agent IDs (primary + registered agents).
pub fn list_agent_ids(state: &ServerState) -> Vec<String> {
    let registry = state.agent.agent_registry();
    let mut ids = vec![PRIMARY_AGENT_ID.to_string()];
    ids.extend(registry.list_agents().into_iter().map(|info| info.id));
    ids
}

/// Build the list of agent info for the UI.
pub fn build_agent_list(state: &ServerState) -> Vec<UiAgentInfo> {
    let mut agents = Vec::new();
    agents.push(UiAgentInfo {
        id: PRIMARY_AGENT_ID.to_string(),
        name: "Primary Agent".to_string(),
        description: "Main agent for the current session.".to_string(),
        capabilities: Vec::new(),
    });
    let registry = state.agent.agent_registry();
    for info in registry.list_agents() {
        agents.push(UiAgentInfo {
            id: info.id,
            name: info.name,
            description: info.description,
            capabilities: info.capabilities,
        });
    }
    agents
}

/// Get the working directory for a session.
pub async fn session_cwd_for(state: &ServerState, session_id: &str) -> Option<PathBuf> {
    let cwds = state.session_cwds.lock().await;
    cwds.get(session_id).cloned()
}

/// Resolve and normalize a working directory path.
pub fn resolve_cwd(cwd: Option<String>) -> Option<PathBuf> {
    cwd.map(|path| normalize_cwd(&PathBuf::from(path)))
}

/// Collect event sources from the agent and its registry.
pub fn collect_event_sources(agent: &Arc<AgentHandle>) -> Vec<Arc<crate::event_bus::EventBus>> {
    // Delegate to the shared implementation in acp/shared.rs
    crate::acp::shared::collect_event_sources(agent)
}
