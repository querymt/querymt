//! The `AgentHandle` trait, in its own file so that edits elsewhere in the old
//! module do not move its definition (a shifted `dyn` trait span costs about a
//! minute of rebuild in the crates above).

use crate::acp::protocol::{
    CancelNotification, Error, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse,
};
use crate::agent::remote::SessionActorRef;
use crate::delegation::AgentRegistry;
use async_trait::async_trait;
use querymt_agent_core::events::{AgentEventKind, EventEnvelope};
use querymt_agent_services::event_fanout::EventFanout;
use std::any::Any;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Trait capturing the interface consumers actually use for agent interaction.
///
/// Both `LocalAgentHandle` (local agent) and `RemoteAgentHandle` (remote agent)
/// implement this trait. The registry, quorum, delegation orchestrator, and UI
/// all work with `Arc<dyn AgentHandle>`.
#[async_trait]
pub trait AgentHandle: Send + Sync {
    // --- Session management ---

    /// Create a new session. Returns session_id.
    async fn new_session(
        &self,
        req: NewSessionRequest,
    ) -> std::result::Result<NewSessionResponse, Error>;

    /// Send a prompt to a session.
    async fn prompt(&self, req: PromptRequest) -> std::result::Result<PromptResponse, Error>;

    /// Cancel an ongoing prompt.
    async fn cancel(&self, notif: CancelNotification) -> std::result::Result<(), Error>;

    /// Load an existing session into this agent runtime.
    async fn load_session(
        &self,
        req: LoadSessionRequest,
    ) -> std::result::Result<LoadSessionResponse, Error>;

    /// Create a session for delegation. Returns both session_id and a
    /// SessionActorRef for direct kameo messaging (planning context, history).
    async fn create_delegation_session(
        &self,
        cwd: Option<String>,
        parent_session_id: String,
    ) -> std::result::Result<(String, SessionActorRef), Error>;

    /// Set persisted lifecycle state without loading the session actor.
    async fn set_session_lifecycle(
        &self,
        session_id: &str,
        patch: crate::session::lifecycle::SessionLifecyclePatch,
    ) -> crate::session::error::SessionResult<crate::session::lifecycle::SessionLifecycle> {
        let _ = (session_id, patch);
        Err(crate::session::error::SessionError::InvalidOperation(
            "Session lifecycle updates are unsupported by this agent handle".into(),
        ))
    }

    /// Whether this handle can execute tools in a node-local managed workspace.
    fn supports_local_workspaces(&self) -> bool {
        false
    }

    async fn create_isolated_delegation_session(
        &self,
        _workspace: crate::worktree::WorkspaceBinding,
        _parent_session_id: String,
    ) -> std::result::Result<(String, SessionActorRef), Error> {
        Err(Error::invalid_request().data("isolated delegation requires local workspace execution"))
    }

    // --- Event system ---

    /// Subscribe to agent events.
    fn subscribe_events(&self) -> broadcast::Receiver<EventEnvelope>;

    /// Get the event fanout.
    fn event_fanout(&self) -> &Arc<EventFanout>;

    /// Emit an event.
    fn emit_event(&self, session_id: &str, kind: AgentEventKind);

    // --- Registry access ---

    /// Get the agent/delegation registry.
    fn agent_registry(&self) -> Arc<dyn AgentRegistry + Send + Sync>;

    // --- Remote mesh routing ---

    /// Set the mesh handle for provider routing.
    ///
    /// Default implementation is a no-op. `LocalAgentHandle` overrides
    /// to store the mesh handle on its `SessionProvider`, enabling
    /// `MeshChatProvider` creation for sessions on this agent.
    #[cfg(feature = "remote")]
    fn set_mesh_handle(&self, _mesh: crate::agent::remote::MeshHandle) {}

    // --- Downcasting (transitional) ---

    async fn shutdown(&self) {}

    /// For downcasting to concrete types. Transitional — should be eliminated
    /// over time by moving needed methods onto the trait.
    fn as_any(&self) -> &dyn Any;
}
