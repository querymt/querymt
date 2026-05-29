//! Session management and routing mode logic.
//!
//! Handles session creation, agent lookup, routing modes (single/broadcast),
//! and session-related state management.

use super::error::format_prefixed_error_chain;
use super::messages::{RoutingMode, UiAgentInfo, UiProfileInfo, UiPromptBlock, UiServerMessage};
use super::{ServerState, cursor_from_events};
use crate::agent::LocalAgentHandle as AgentHandle;
use crate::agent::core::AgentMode;
use crate::agent::handle::AgentHandle as AgentHandleTrait;
use crate::agent::remote::SessionActorRef;
use crate::events::EventEnvelope;
use crate::index::{normalize_cwd, resolve_workspace_root};
use agent_client_protocol::schema::{
    ContentBlock, LoadSessionRequest, NewSessionRequest, PromptRequest, SessionId,
};
use querymt::chat::ReasoningEffort;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub const PRIMARY_AGENT_ID: &str = "primary";

/// Ensure sessions exist for the current routing mode.
pub async fn ensure_sessions_for_mode_with_profile(
    state: &ServerState,
    conn_id: &str,
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
    request_id: Option<&str>,
    profile_id: Option<&str>,
) -> Result<(), String> {
    let mode = current_mode(state, conn_id).await?;
    match mode {
        RoutingMode::Single => {
            let agent_id = current_active_agent(state, conn_id).await?;
            ensure_session(state, conn_id, &agent_id, cwd, tx, request_id, profile_id).await?;
        }
        RoutingMode::Broadcast => {
            let agent_ids = list_agent_ids(state).await;
            for (i, agent_id) in agent_ids.iter().enumerate() {
                // Only pass request_id to the first agent in broadcast mode
                let req_id = if i == 0 { request_id } else { None };
                ensure_session(state, conn_id, agent_id, cwd, tx, req_id, profile_id).await?;
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
            let session_id = ensure_session(state, conn_id, &agent_id, cwd, tx, None, None).await?;
            prompt_session(state, &session_id, prompt, cwd).await?;
        }
        RoutingMode::Broadcast => {
            let agent_ids = list_agent_ids(state).await;
            for agent_id in agent_ids {
                let session_id =
                    ensure_session(state, conn_id, &agent_id, cwd, tx, None, None).await?;
                prompt_session(state, &session_id, prompt, cwd).await?;
            }
        }
    }
    Ok(())
}

async fn prompt_session(
    state: &ServerState,
    session_id: &str,
    prompt: &[UiPromptBlock],
    cwd: Option<&PathBuf>,
) -> Result<(), String> {
    let session_ref = session_ref_for_session(state, session_id)
        .await
        .ok_or_else(|| {
            format_prompt_error(session_id, "unresolved session", "session not found")
        })?;
    let session_cwd = session_cwd_for(state, session_id).await.or(cwd.cloned());
    let prompt_blocks = super::mentions::build_prompt_blocks(
        &state.workspace_manager,
        session_cwd.as_ref(),
        prompt,
        Some(&session_ref),
    )
    .await;
    let prompt_target = prompt_target_for_session_ref(&session_ref);
    send_prompt(session_ref, session_id, prompt_blocks, &prompt_target).await
}

fn prompt_target_for_session_ref(session_ref: &SessionActorRef) -> String {
    if session_ref.is_remote() {
        format!("remote node '{}'", session_ref.node_label())
    } else {
        "local session".to_string()
    }
}

/// Send a prompt to a specific session actor.
async fn send_prompt(
    session_ref: SessionActorRef,
    session_id: &str,
    prompt: Vec<ContentBlock>,
    prompt_target: &str,
) -> Result<(), String> {
    let request = PromptRequest::new(session_id.to_string(), prompt);
    session_ref
        .prompt(request)
        .await
        .map_err(|err| format_prompt_error(session_id, prompt_target, &err.message))?;
    Ok(())
}

fn format_prompt_error(session_id: &str, prompt_target: &str, message: &str) -> String {
    format!(
        "Failed to prompt session '{}' ({prompt_target}): {}",
        session_id, message
    )
}

/// Ensure a session exists for the given agent, creating one if needed.
pub async fn ensure_session(
    state: &ServerState,
    conn_id: &str,
    agent_id: &str,
    cwd: Option<&PathBuf>,
    tx: &mpsc::Sender<String>,
    request_id: Option<&str>,
    profile_id: Option<&str>,
) -> Result<String, String> {
    let existing = {
        let connections = state.connections.lock().await;
        connections
            .get(conn_id)
            .and_then(|conn| conn.sessions.get(agent_id).cloned())
    };
    if let Some(session_id) = &existing {
        let root_session_ref = {
            let registry = state.agent.registry.lock().await;
            registry.get(session_id).cloned()
        };
        if root_session_ref
            .as_ref()
            .is_some_and(SessionActorRef::is_remote)
        {
            return Ok(session_id.clone());
        }

        // Verify the session still exists in the registry.
        // After a force-stop the session is removed, making the binding stale.
        let existing_profile_id =
            resolve_profile_id_for_session(state, Some(session_id), profile_id).await?;
        let still_alive = if let Ok(agent) =
            agent_for_profile_and_id(state, existing_profile_id.as_deref(), agent_id).await
        {
            agent
                .load_session(LoadSessionRequest::new(
                    SessionId::from(session_id.clone()),
                    PathBuf::new(),
                ))
                .await
                .is_ok()
        } else {
            false
        };
        if still_alive {
            return Ok(session_id.clone());
        }
        // Session was destroyed (force-stopped) — clear the stale binding
        // so the code below creates a fresh session.
        {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.sessions.remove(agent_id);
            }
        }
    }

    let selected_profile_id = resolve_profile_id(state, profile_id).await?;
    let agent = agent_for_profile_and_id(state, selected_profile_id.as_deref(), agent_id).await?;

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

    if let (Some(profiles), Some(profile_id)) = (&state.profiles, selected_profile_id.as_deref()) {
        profiles
            .bind_session_to_profile(session_id.clone(), profile_id)
            .await
            .map_err(|err| format!("Failed to bind session to profile: {err}"))?;
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
            profile_id: selected_profile_id.clone(),
            session_id: session_id.clone(),
            request_id: request_id.map(|s| s.to_string()),
        },
    )
    .await;

    // Replay stored events for the new session (includes ProviderChanged)
    // No child sessions for a new session
    if let Ok(audit) = state.view_store.get_audit_view(&session_id, false).await {
        let cursor = cursor_from_events(&audit.events);
        let events: Vec<EventEnvelope> = audit.events.into_iter().map(Into::into).collect();

        {
            let mut connections = state.connections.lock().await;
            if let Some(conn) = connections.get_mut(conn_id) {
                conn.session_cursors
                    .insert(session_id.clone(), cursor.clone());
            }
        }

        let _ = super::connection::send_message(
            tx,
            UiServerMessage::SessionEvents {
                session_id: session_id.clone(),
                agent_id: agent_id.to_string(),
                profile_id: selected_profile_id.clone(),
                events,
                cursor,
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

/// Get an agent handle by ID.
pub fn agent_for_id(state: &ServerState, agent_id: &str) -> Option<Arc<dyn AgentHandleTrait>> {
    if agent_id == PRIMARY_AGENT_ID {
        return Some(state.agent.clone() as Arc<dyn AgentHandleTrait>);
    }
    let registry = state.agent.agent_registry();
    registry.get_handle(agent_id)
}

pub async fn resolve_profile_id(
    state: &ServerState,
    profile_id: Option<&str>,
) -> Result<Option<String>, String> {
    resolve_profile_id_for_session(state, None, profile_id).await
}

pub async fn resolve_profile_id_for_session(
    state: &ServerState,
    session_id: Option<&str>,
    requested_profile_id: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(profiles) = &state.profiles {
        if let Some(session_id) = session_id
            && let Some(binding) = profiles.session_binding(session_id).await
        {
            return Ok(Some(binding.profile_id));
        }

        if let Some(id) = requested_profile_id {
            return Ok(Some(id.to_string()));
        }

        return Ok(Some(profiles.active_profile_id().await));
    }

    match requested_profile_id {
        Some("default") => Ok(Some("default".to_string())),
        Some(id) => Err(format!("Unknown profile: {id}")),
        None => Ok(None),
    }
}

pub async fn agent_for_profile_and_id(
    state: &ServerState,
    profile_id: Option<&str>,
    agent_id: &str,
) -> Result<Arc<dyn AgentHandleTrait>, String> {
    let Some(profiles) = &state.profiles else {
        return agent_for_id(state, agent_id).ok_or_else(|| format!("Unknown agent: {agent_id}"));
    };

    let profile_id = match profile_id {
        Some(id) => id.to_string(),
        None => profiles.active_profile_id().await,
    };
    let runtime = profiles
        .runtime_for_profile(&profile_id)
        .await
        .map_err(|err| {
            format_prefixed_error_chain(&format!("Failed to load profile '{profile_id}'"), &err)
        })?;
    let handle = runtime.agent().handle();
    if agent_id == PRIMARY_AGENT_ID {
        Ok(handle as Arc<dyn AgentHandleTrait>)
    } else {
        handle
            .agent_registry()
            .get_handle(agent_id)
            .ok_or_else(|| format!("Unknown agent: {agent_id}"))
    }
}

pub async fn local_agent_for_profile(
    state: &ServerState,
    profile_id: Option<&str>,
) -> Result<Arc<AgentHandle>, String> {
    let Some(profiles) = &state.profiles else {
        return Ok(state.agent.clone());
    };

    let profile_id = match profile_id {
        Some(id) => id.to_string(),
        None => profiles.active_profile_id().await,
    };
    let runtime = profiles
        .runtime_for_profile(&profile_id)
        .await
        .map_err(|err| {
            format_prefixed_error_chain(&format!("Failed to load profile '{profile_id}'"), &err)
        })?;
    Ok(runtime.agent().handle())
}

pub async fn local_agent_for_session(
    state: &ServerState,
    session_id: Option<&str>,
    requested_profile_id: Option<&str>,
) -> Result<Arc<AgentHandle>, String> {
    let profile_id =
        resolve_profile_id_for_session(state, session_id, requested_profile_id).await?;
    local_agent_for_profile(state, profile_id.as_deref()).await
}

pub async fn session_ref_for_profile(
    state: &ServerState,
    profile_id: Option<&str>,
    session_id: &str,
) -> Option<SessionActorRef> {
    let local_agent = local_agent_for_profile(state, profile_id).await.ok()?;
    let registry = local_agent.registry.lock().await;
    registry.get(session_id).cloned()
}

pub async fn session_ref_for_session(
    state: &ServerState,
    session_id: &str,
) -> Option<SessionActorRef> {
    let root_session_ref = {
        let registry = state.agent.registry.lock().await;
        registry.get(session_id).cloned()
    };

    if root_session_ref
        .as_ref()
        .is_some_and(SessionActorRef::is_remote)
    {
        return root_session_ref;
    }

    let profile_id = resolve_profile_id_for_session(state, Some(session_id), None)
        .await
        .ok()?;
    session_ref_for_profile(state, profile_id.as_deref(), session_id)
        .await
        .or(root_session_ref)
}

pub async fn default_mode_for_session(state: &ServerState, session_id: Option<&str>) -> AgentMode {
    local_agent_for_session(state, session_id, None)
        .await
        .ok()
        .and_then(|agent| agent.default_mode.lock().map(|mode| *mode).ok())
        .unwrap_or(AgentMode::Build)
}

pub async fn default_reasoning_effort_for_session(
    state: &ServerState,
    session_id: Option<&str>,
) -> Option<ReasoningEffort> {
    local_agent_for_session(state, session_id, None)
        .await
        .ok()
        .and_then(|agent| **agent.default_reasoning_effort.load())
}

pub async fn mode_for_session(state: &ServerState, session_id: Option<&str>) -> AgentMode {
    if let Some(session_id) = session_id
        && let Some(session_ref) = session_ref_for_session(state, session_id).await
        && let Ok(mode) = session_ref.get_mode().await
    {
        return mode;
    }

    default_mode_for_session(state, session_id).await
}

pub async fn reasoning_effort_for_session(
    state: &ServerState,
    session_id: Option<&str>,
) -> Option<ReasoningEffort> {
    if let Some(session_id) = session_id
        && let Some(session_ref) = session_ref_for_session(state, session_id).await
        && let Ok(effort) = session_ref.get_reasoning_effort().await
    {
        return effort;
    }

    default_reasoning_effort_for_session(state, session_id).await
}

pub async fn list_profiles(state: &ServerState) -> Result<Vec<UiProfileInfo>, String> {
    if let Some(profiles) = &state.profiles {
        profiles
            .list_profiles()
            .await
            .map(|profiles| profiles.into_iter().map(Into::into).collect())
            .map_err(|err| format_prefixed_error_chain("Failed to list profiles", &err))
    } else {
        Ok(Vec::new())
    }
}

pub async fn active_profile_id(state: &ServerState) -> Option<String> {
    if let Some(profiles) = &state.profiles {
        Some(profiles.active_profile_id().await)
    } else {
        None
    }
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
pub async fn list_agent_ids(state: &ServerState) -> Vec<String> {
    build_agent_list(state)
        .await
        .into_iter()
        .map(|info| info.id)
        .collect()
}

/// Build the list of agent info for the UI.
pub async fn build_agent_list(state: &ServerState) -> Vec<UiAgentInfo> {
    let registry = if let Some(profiles) = &state.profiles {
        profiles
            .active_runtime()
            .await
            .map(|runtime| runtime.agent().handle().agent_registry())
            .unwrap_or_else(|err| {
                log::warn!("Failed to load active profile for UI agent list: {err}");
                state.agent.agent_registry()
            })
    } else {
        state.agent.agent_registry()
    };

    let mut agents = Vec::new();
    agents.push(UiAgentInfo {
        id: PRIMARY_AGENT_ID.to_string(),
        name: "Primary Agent".to_string(),
        description: "Main agent for the current session.".to_string(),
        capabilities: Vec::new(),
    });
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
pub fn collect_event_sources(
    agent: &Arc<AgentHandle>,
) -> Vec<Arc<crate::event_fanout::EventFanout>> {
    // Delegate to the shared implementation in acp/shared.rs
    crate::acp::shared::collect_event_sources(agent)
}

#[cfg(test)]
mod tests {
    use super::{
        PRIMARY_AGENT_ID, default_mode_for_session, default_reasoning_effort_for_session,
        ensure_session, format_prompt_error, prompt_for_mode, resolve_profile_id_for_session,
        session_ref_for_session,
    };
    use crate::agent::core::AgentMode;
    use crate::api::AgentInfra;
    use crate::profiles::{LocalProfileCatalog, ProfileCatalog, ProfileRuntimeManager};
    use crate::test_utils::{TestServerState, empty_plugin_registry};
    use crate::ui::messages::UiPromptBlock;
    #[cfg(feature = "remote")]
    use crate::{
        agent::SessionActor, agent::core::SessionRuntime,
        agent::remote::test_helpers::fixtures::get_test_mesh,
    };
    #[cfg(feature = "remote")]
    use kameo::actor::Spawn;
    use std::path::Path;
    use std::sync::Arc;
    #[cfg(feature = "remote")]
    use uuid::Uuid;

    fn write_profile(dir: &Path, name: &str) {
        std::fs::write(
            dir.join(name),
            r#"
[agent]
provider = "test"
model = "test-model"
system = "inline"
"#,
        )
        .expect("profile should be written");
    }

    #[test]
    fn format_prompt_error_includes_session_and_target() {
        let msg = format_prompt_error("sess-123", "remote node 'gpu-box'", "timeout");
        assert!(msg.contains("sess-123"));
        assert!(msg.contains("remote node 'gpu-box'"));
        assert!(msg.contains("timeout"));
    }

    #[test]
    fn format_prompt_error_preserves_backend_message() {
        let raw = "session not found";
        let msg = format_prompt_error("sess-9", "local session", raw);
        assert!(msg.ends_with(raw));
    }

    async fn profile_fixture() -> (
        TestServerState,
        Arc<ProfileRuntimeManager<Arc<dyn ProfileCatalog>>>,
        tempfile::TempDir,
    ) {
        let mut fixture = TestServerState::new().await;
        let dir = tempfile::TempDir::new().expect("temp profile dir");
        write_profile(dir.path(), "alpha.toml");
        write_profile(dir.path(), "beta.toml");
        let catalog: Arc<dyn ProfileCatalog> = Arc::new(
            LocalProfileCatalog::builder()
                .include_embedded_default(false)
                .local_dir(dir.path())
                .build(),
        );
        let (registry, _registry_dir) = empty_plugin_registry().expect("empty plugin registry");
        let infra = AgentInfra {
            plugin_registry: Arc::new(registry),
            storage: Some(fixture.agent.storage.clone()),
            session_mcp_attachment_source: None,
        };
        let profiles = Arc::new(ProfileRuntimeManager::with_infra_boxed(
            catalog, "alpha", infra,
        ));
        fixture.state.profiles = Some(profiles.clone());
        (fixture, profiles, dir)
    }

    #[tokio::test]
    async fn resolve_profile_for_session_prefers_existing_binding_over_active_or_requested() {
        let (fixture, profiles, _dir) = profile_fixture().await;

        profiles
            .bind_session_to_profile("session-1", "alpha")
            .await
            .expect("session should bind to alpha profile");
        profiles
            .set_active_profile("beta")
            .await
            .expect("beta should become active for new sessions");

        let resolved =
            resolve_profile_id_for_session(&fixture.state, Some("session-1"), Some("beta"))
                .await
                .expect("bound profile should resolve");

        assert_eq!(resolved.as_deref(), Some("alpha"));
    }

    #[tokio::test]
    async fn session_defaults_follow_bound_profile_runtime() {
        let (fixture, profiles, _dir) = profile_fixture().await;

        profiles
            .bind_session_to_profile("session-1", "alpha")
            .await
            .expect("session should bind to alpha profile");
        profiles
            .set_active_profile("beta")
            .await
            .expect("beta should become active for new sessions");

        let beta = profiles
            .runtime_for_profile("beta")
            .await
            .expect("beta runtime should load")
            .agent()
            .handle();
        *beta.default_mode.lock().expect("mode lock") = AgentMode::Review;
        beta.default_reasoning_effort
            .store(Arc::new(Some(querymt::chat::ReasoningEffort::High)));

        let alpha = profiles
            .runtime_for_profile("alpha")
            .await
            .expect("alpha runtime should load")
            .agent()
            .handle();
        *alpha.default_mode.lock().expect("mode lock") = AgentMode::Plan;
        alpha
            .default_reasoning_effort
            .store(Arc::new(Some(querymt::chat::ReasoningEffort::Low)));

        assert_eq!(
            default_mode_for_session(&fixture.state, Some("session-1")).await,
            AgentMode::Plan
        );
        assert_eq!(
            default_reasoning_effort_for_session(&fixture.state, Some("session-1")).await,
            Some(querymt::chat::ReasoningEffort::Low)
        );
    }

    #[cfg(feature = "remote")]
    async fn attach_test_remote_session(fixture: &TestServerState, session_id: &str) {
        let mesh = get_test_mesh().await.clone();
        let runtime = SessionRuntime::new(
            None,
            Default::default(),
            crate::agent::core::McpToolState::empty(),
        );
        let actor = SessionActor::new(
            fixture.agent.config.clone(),
            session_id.to_string(),
            runtime,
        )
        .with_mesh(Some(mesh.clone()));
        let local_ref = SessionActor::spawn(actor);
        let dht_name = crate::agent::remote::scope::scoped_session(
            &crate::agent::remote::scope::MeshScopeId::lan_default(),
            session_id,
        );
        mesh.register_actor(local_ref, dht_name.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let remote_ref = mesh
            .lookup_actor::<SessionActor>(&dht_name)
            .await
            .expect("DHT lookup should succeed")
            .expect("remote actor should be available");

        fixture
            .agent
            .handle
            .attach_remote_session(
                session_id.to_string(),
                remote_ref,
                "remote-peer".to_string(),
                None,
                None,
            )
            .await;
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn session_ref_for_session_prefers_root_remote_actor_over_local_profile_runtime() {
        let (fixture, profiles, _dir) = profile_fixture().await;
        let session_id = format!("remote-profile-{}", Uuid::now_v7());
        attach_test_remote_session(&fixture, &session_id).await;

        profiles
            .bind_session_to_profile(&session_id, "alpha")
            .await
            .expect("session should bind to active local profile");
        profiles
            .set_active_profile("beta")
            .await
            .expect("beta should become active for new sessions");

        let session_ref = session_ref_for_session(&fixture.state, &session_id)
            .await
            .expect("remote session should resolve");
        assert!(session_ref.is_remote());
        assert_eq!(session_ref.node_label(), "remote-peer");
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn ensure_session_reuses_attached_remote_session_without_profile_runtime() {
        let (fixture, profiles, _dir) = profile_fixture().await;
        let conn_id = "conn-remote-ensure";
        let (tx, _rx) = fixture.add_connection(conn_id).await;
        let session_id = format!("remote-ensure-{}", Uuid::now_v7());
        attach_test_remote_session(&fixture, &session_id).await;
        profiles
            .set_active_profile("beta")
            .await
            .expect("beta should become active for new sessions");

        {
            let mut connections = fixture.state.connections.lock().await;
            let conn = connections.get_mut(conn_id).expect("connection exists");
            conn.sessions
                .insert(PRIMARY_AGENT_ID.to_string(), session_id.clone());
        }

        let resolved = ensure_session(
            &fixture.state,
            conn_id,
            PRIMARY_AGENT_ID,
            None,
            &tx,
            None,
            None,
        )
        .await
        .expect("attached remote session should be reused");

        assert_eq!(resolved, session_id);
        assert!(profiles.session_binding(&resolved).await.is_none());
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn prompt_for_mode_uses_root_remote_actor_when_connection_session_is_remote() {
        let (fixture, _profiles, _dir) = profile_fixture().await;
        let conn_id = "conn-remote-prompt";
        let (tx, _rx) = fixture.add_connection(conn_id).await;
        let session_id = format!("remote-prompt-{}", Uuid::now_v7());
        attach_test_remote_session(&fixture, &session_id).await;

        {
            let mut connections = fixture.state.connections.lock().await;
            let conn = connections.get_mut(conn_id).expect("connection exists");
            conn.sessions
                .insert(PRIMARY_AGENT_ID.to_string(), session_id.clone());
        }

        let err = prompt_for_mode(
            &fixture.state,
            conn_id,
            &[UiPromptBlock::Text {
                text: "hello remote".to_string(),
            }],
            None,
            &tx,
        )
        .await
        .expect_err("test remote actor has no provider, but prompt should target remote actor");

        assert!(
            err.contains("remote node 'remote-peer'"),
            "expected remote prompt target, got: {err}"
        );
        assert!(
            !err.contains("local session"),
            "remote prompt must not be routed through local profile runtime: {err}"
        );
    }
}
