//! `SessionActorRef` — location-transparent reference to a `SessionActor`.
//!
//! Every place that currently holds `ActorRef<SessionActor>` switches to
//! `SessionActorRef`. For the `Local` variant, each method delegates to
//! `actor_ref.ask(msg).await`. For the `Remote` variant (behind the `remote`
//! feature), each method delegates to `remote_ref.ask(&msg).await`.

use crate::agent::core::AgentMode;
use crate::agent::file_proxy::{FileProxyError, GetFileIndexResponse, ReadRemoteFileResponse};
use crate::agent::messages;
use crate::agent::session_actor::SessionActor;
use crate::agent::undo::{RedoResult, UndoError, UndoResult};
use crate::error::AgentError;
use crate::events::SessionLimits;
use crate::model::AgentMessage;
use crate::session::store::LLMConfig;
use agent_client_protocol::{
    Error as AcpError, PromptRequest, PromptResponse, SetSessionModelResponse,
};
use kameo::actor::ActorRef;

/// Location-transparent reference to a `SessionActor`.
///
/// Wraps either a local `ActorRef<SessionActor>` or (with the `remote` feature)
/// a `RemoteActorRef<SessionActor>`. All public methods have identical signatures
/// regardless of variant — the transport is an implementation detail.
#[derive(Clone, Debug)]
pub enum SessionActorRef {
    /// Actor lives in this process.
    Local(ActorRef<SessionActor>),

    /// Actor lives on a remote node in the kameo mesh.
    #[cfg(feature = "remote")]
    Remote {
        actor_ref: kameo::actor::RemoteActorRef<SessionActor>,
        /// Human-readable label for the peer, e.g. "dev-gpu". Used in UI display.
        peer_label: String,
    },
}

// ── Constructors ─────────────────────────────────────────────────────────

impl From<ActorRef<SessionActor>> for SessionActorRef {
    fn from(actor_ref: ActorRef<SessionActor>) -> Self {
        Self::Local(actor_ref)
    }
}

#[cfg(feature = "remote")]
impl SessionActorRef {
    /// Create a remote session actor reference.
    pub fn remote(
        actor_ref: kameo::actor::RemoteActorRef<SessionActor>,
        peer_label: String,
    ) -> Self {
        Self::Remote {
            actor_ref,
            peer_label,
        }
    }
}

// ── Identification ───────────────────────────────────────────────────────

impl SessionActorRef {
    /// Whether this reference points to a remote actor.
    pub fn is_remote(&self) -> bool {
        match self {
            Self::Local(_) => false,
            #[cfg(feature = "remote")]
            Self::Remote { .. } => true,
        }
    }

    /// Human-readable label for the node hosting this session.
    pub fn node_label(&self) -> &str {
        match self {
            Self::Local(_) => "local",
            #[cfg(feature = "remote")]
            Self::Remote { peer_label, .. } => peer_label,
        }
    }
}

// ── Message dispatch ─────────────────────────────────────────────────────
//
// Each method mirrors an `actor_ref.ask(Msg)` or `actor_ref.tell(Msg)` call.
// For the Local variant we forward directly. For Remote, we forward through
// the kameo remote transport.
//
// Error handling: local `SendError` and remote `RemoteSendError` are both
// mapped to `AgentError::RemoteActor` for a uniform API.

impl SessionActorRef {
    /// Send a prompt to the session and wait for completion.
    #[tracing::instrument(
        name = "remote.session_ref.prompt",
        skip(self, req),
        fields(
            is_remote = self.is_remote(),
            peer_label = %self.node_label(),
            timed_out = tracing::field::Empty,
        )
    )]
    pub async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::Prompt { req })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => {
                // Use a generous timeout (10 min) to prevent indefinite hangs
                // when the remote node is unreachable or the prompt execution stalls.
                const REMOTE_PROMPT_TIMEOUT: std::time::Duration =
                    std::time::Duration::from_secs(600);
                match tokio::time::timeout(
                    REMOTE_PROMPT_TIMEOUT,
                    actor_ref.ask(&messages::Prompt { req }),
                )
                .await
                {
                    Ok(result) => {
                        tracing::Span::current().record("timed_out", false);
                        result.map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string())))
                    }
                    Err(_elapsed) => {
                        tracing::Span::current().record("timed_out", true);
                        Err(AcpError::from(AgentError::SessionTimeout {
                            details: "Remote prompt timed out (exceeded 600s)".to_string(),
                        }))
                    }
                }
            }
        }
    }

    /// Fire-and-forget cancellation.
    pub async fn cancel(&self) -> Result<(), AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .tell(messages::Cancel)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .tell(&messages::Cancel)
                .send()
                .map_err(|e| AgentError::RemoteActor(e.to_string())),
        }
    }

    /// Set the agent mode for this session.
    pub async fn set_mode(&self, mode: AgentMode) -> Result<(), AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .tell(messages::SetMode { mode })
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .tell(&messages::SetMode { mode })
                .send()
                .map_err(|e| AgentError::RemoteActor(e.to_string())),
        }
    }

    /// Get the current agent mode.
    pub async fn get_mode(&self) -> Result<AgentMode, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetMode)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetMode)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),
        }
    }

    /// Undo filesystem changes back to a specific message.
    ///
    /// Works for both local and remote sessions. For remote sessions the handler
    /// runs on the remote node (where the filesystem lives) and returns the result
    /// over the kameo mesh.
    pub async fn undo(&self, message_id: String) -> Result<UndoResult, UndoError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::Undo { message_id })
                .await
                .map_err(|e| UndoError::ActorSend(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::Undo { message_id })
                .await
                .map_err(|e| UndoError::ActorSend(e.to_string())),
        }
    }

    /// Redo: restore to pre-undo state.
    ///
    /// Works for both local and remote sessions. For remote sessions the handler
    /// runs on the remote node (where the filesystem lives) and returns the result
    /// over the kameo mesh.
    pub async fn redo(&self) -> Result<RedoResult, UndoError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::Redo)
                .await
                .map_err(|e| UndoError::ActorSend(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::Redo)
                .await
                .map_err(|e| UndoError::ActorSend(e.to_string())),
        }
    }

    /// Set session model via ACP protocol.
    pub async fn set_session_model(
        &self,
        req: agent_client_protocol::SetSessionModelRequest,
    ) -> Result<SetSessionModelResponse, AcpError> {
        self.set_session_model_with_node(messages::SetSessionModel {
            req,
            provider_node_id: None,
        })
        .await
    }

    /// Set session model with an optional provider node (for mesh-remote providers).
    ///
    /// Routes directly through the `SessionActorRef` so remote sessions work correctly,
    /// unlike the old path that went through a stub and returned an error.
    pub async fn set_session_model_with_node(
        &self,
        msg: messages::SetSessionModel,
    ) -> Result<SetSessionModelResponse, AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(msg)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&msg)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),
        }
    }

    /// Get session history. Works for both local and remote sessions.
    #[tracing::instrument(
        name = "remote.session_ref.get_history",
        skip(self),
        fields(is_remote = self.is_remote(), peer_label = %self.node_label())
    )]
    pub async fn get_history(&self) -> Result<Vec<AgentMessage>, AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetHistory)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetHistory)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),
        }
    }

    /// Get current LLM config for this session.
    pub async fn get_llm_config(&self) -> Result<Option<LLMConfig>, AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetLlmConfig)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetLlmConfig)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),
        }
    }

    /// Get session limits.
    pub async fn get_session_limits(&self) -> Result<Option<SessionLimits>, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetSessionLimits)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetSessionLimits)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),
        }
    }

    /// Set the client bridge for SessionUpdate notifications (local only).
    ///
    /// This is a no-op for remote sessions — the bridge is a local channel
    /// that cannot be serialized across the network. Remote sessions use
    /// the EventRelay mechanism instead.
    pub async fn set_bridge(
        &self,
        bridge: crate::acp::client_bridge::ClientBridgeSender,
    ) -> Result<(), AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .tell(messages::SetBridge { bridge })
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { .. } => {
                // Remote sessions use EventRelay, not direct bridge
                log::debug!("set_bridge called on remote SessionActorRef — ignored");
                Ok(())
            }
        }
    }

    /// Shutdown this session actor gracefully (local only for now).
    pub async fn shutdown(&self) -> Result<(), AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .tell(messages::Shutdown)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { .. } => {
                log::warn!("shutdown called on remote SessionActorRef — not yet supported");
                Ok(())
            }
        }
    }

    /// Subscribe to events from this session (for remote event relay).
    ///
    /// Registers an event forwarder on the session that sends events to the
    /// specified relay actor (identified by its ActorId as u64).
    #[tracing::instrument(
        name = "remote.session_ref.subscribe_events",
        skip(self),
        fields(
            is_remote = self.is_remote(),
            peer_label = %self.node_label(),
            relay_actor_id,
        )
    )]
    pub async fn subscribe_events(&self, relay_actor_id: u64) -> Result<(), AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::SubscribeEvents { relay_actor_id })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::SubscribeEvents { relay_actor_id })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),
        }
    }

    /// Set planning context on a delegate session.
    ///
    /// Injects the parent session's planning summary into this session's system prompt.
    /// Used by the delegation orchestrator to provide context without requiring direct
    /// access to the child's `SessionStore`.
    pub async fn set_planning_context(&self, summary: String) -> Result<(), AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::SetPlanningContext { summary })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string())))?,

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::SetPlanningContext { summary })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string())))?,
        }
        Ok(())
    }

    /// Unsubscribe from events (remove event forwarder).
    pub async fn unsubscribe_events(&self, relay_actor_id: u64) -> Result<(), AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::UnsubscribeEvents { relay_actor_id })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::UnsubscribeEvents { relay_actor_id })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),
        }
    }

    /// Get the file index from this session's workspace.
    ///
    /// Works for both local and remote sessions. For remote sessions the handler
    /// reads the workspace index on the remote node and returns it over the mesh.
    #[tracing::instrument(
        name = "remote.session_ref.get_file_index",
        skip(self),
        fields(is_remote = self.is_remote(), peer_label = %self.node_label())
    )]
    pub async fn get_file_index(&self) -> Result<GetFileIndexResponse, FileProxyError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetFileIndex)
                .await
                .map_err(|e| FileProxyError::ActorSend(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetFileIndex)
                .await
                .map_err(|e| FileProxyError::ActorSend(e.to_string())),
        }
    }

    /// Read a file or directory on this session's node.
    ///
    /// Works for both local and remote sessions. For remote sessions the handler
    /// reads from the remote filesystem and returns content over the mesh.
    #[tracing::instrument(
        name = "remote.session_ref.read_remote_file",
        skip(self),
        fields(
            is_remote = self.is_remote(),
            peer_label = %self.node_label(),
            path = %path,
            offset,
            limit,
        )
    )]
    pub async fn read_remote_file(
        &self,
        path: String,
        offset: usize,
        limit: usize,
    ) -> Result<ReadRemoteFileResponse, FileProxyError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::ReadRemoteFile {
                    path,
                    offset,
                    limit,
                })
                .await
                .map_err(|e| FileProxyError::ActorSend(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::ReadRemoteFile {
                    path,
                    offset,
                    limit,
                })
                .await
                .map_err(|e| FileProxyError::ActorSend(e.to_string())),
        }
    }
}
