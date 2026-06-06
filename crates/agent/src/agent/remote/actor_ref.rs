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
use agent_client_protocol::schema::{
    Error as AcpError, PromptRequest, PromptResponse, SetSessionModelResponse,
};
use kameo::actor::ActorRef;
use querymt::chat::ReasoningEffort;
use std::time::Duration;

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
        /// Stable node id for routing/identity; display should continue using `peer_label`.
        remote_node_id: Option<String>,
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
            remote_node_id: None,
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

    #[cfg(feature = "remote")]
    pub fn remote_node_id(&self) -> Option<&str> {
        match self {
            Self::Remote { remote_node_id, .. } => remote_node_id.as_deref(),
            Self::Local(_) => None,
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
    const REMOTE_CONTROL_MAILBOX_TIMEOUT: Duration = Duration::from_secs(10);
    const REMOTE_CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
    const REMOTE_PROMPT_MAILBOX_TIMEOUT: Duration = Duration::from_secs(10);
    const REMOTE_PROMPT_REPLY_TIMEOUT: Duration = Duration::from_secs(600);
    const REMOTE_MODEL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
    const REMOTE_UNDO_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
    const REMOTE_HISTORY_REPLY_TIMEOUT: Duration = Duration::from_secs(60);
    const REMOTE_IO_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

    fn map_infallible_remote_send_error(
        error: kameo::error::RemoteSendError<kameo::error::Infallible>,
    ) -> AgentError {
        match error {
            kameo::error::RemoteSendError::HandlerError(err) => match err {},
            other => AgentError::RemoteActor(other.to_string()),
        }
    }

    fn map_agent_timeout_remote_send_error(
        error: kameo::error::RemoteSendError<AgentError>,
        timeout_message: impl Into<String>,
    ) -> AgentError {
        match error {
            kameo::error::RemoteSendError::HandlerError(err) => err,
            kameo::error::RemoteSendError::ReplyTimeout => AgentError::SessionTimeout {
                details: timeout_message.into(),
            },
            other => AgentError::RemoteActor(other.to_string()),
        }
    }

    fn map_infallible_timeout_remote_send_error(
        error: kameo::error::RemoteSendError<kameo::error::Infallible>,
        timeout_message: impl Into<String>,
    ) -> AgentError {
        match error {
            kameo::error::RemoteSendError::HandlerError(err) => match err {},
            kameo::error::RemoteSendError::ReplyTimeout => AgentError::SessionTimeout {
                details: timeout_message.into(),
            },
            other => AgentError::RemoteActor(other.to_string()),
        }
    }

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
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::Prompt { req })
                .mailbox_timeout(Self::REMOTE_PROMPT_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_PROMPT_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    tracing::Span::current().record(
                        "timed_out",
                        matches!(e, kameo::error::RemoteSendError::ReplyTimeout),
                    );
                    AcpError::from(match e {
                        kameo::error::RemoteSendError::HandlerError(err) => err,
                        kameo::error::RemoteSendError::ReplyTimeout => AgentError::SessionTimeout {
                            details: format!(
                                "Remote prompt timed out (mailbox={}s, reply={}s)",
                                Self::REMOTE_PROMPT_MAILBOX_TIMEOUT.as_secs(),
                                Self::REMOTE_PROMPT_REPLY_TIMEOUT.as_secs()
                            ),
                        },
                        other => AgentError::RemoteActor(other.to_string()),
                    })
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_infallible_timeout_remote_send_error(
                        e,
                        "GetMode timed out on remote session",
                    )
                }),
        }
    }

    /// Set the reasoning effort for this session.
    /// Pass `None` to clear the override and restore model/provider defaults.
    pub async fn set_reasoning_effort(
        &self,
        effort: Option<ReasoningEffort>,
    ) -> Result<(), AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::SetReasoningEffort { effort })
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string()))?,

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::SetReasoningEffort { effort })
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_agent_timeout_remote_send_error(
                        e,
                        "SetReasoningEffort timed out on remote session",
                    )
                })?,
        }
        Ok(())
    }

    /// Get the current reasoning effort.
    pub async fn get_reasoning_effort(&self) -> Result<Option<ReasoningEffort>, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetReasoningEffort)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetReasoningEffort)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_infallible_timeout_remote_send_error(
                        e,
                        "GetReasoningEffort timed out on remote session",
                    )
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_UNDO_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| match e {
                    kameo::error::RemoteSendError::HandlerError(err) => err,
                    kameo::error::RemoteSendError::ReplyTimeout => {
                        UndoError::ActorSend("Undo timed out on remote session".to_string())
                    }
                    other => UndoError::ActorSend(other.to_string()),
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_UNDO_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| match e {
                    kameo::error::RemoteSendError::HandlerError(err) => err,
                    kameo::error::RemoteSendError::ReplyTimeout => {
                        UndoError::ActorSend("Redo timed out on remote session".to_string())
                    }
                    other => UndoError::ActorSend(other.to_string()),
                }),
        }
    }

    /// Fork this session at a specific message boundary.
    ///
    /// Remote forks now go through `RemoteNodeManager` so only local sessions expose
    /// this direct session-actor lifecycle operation.
    pub async fn fork_at_message(&self, message_id: String) -> Result<String, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::ForkAtMessage { message_id })
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { .. } => Err(AgentError::Internal(
                "remote fork must be routed through RemoteNodeManager".to_string(),
            )),
        }
    }

    /// Set session model via ACP protocol.
    pub async fn set_session_model(
        &self,
        req: agent_client_protocol::schema::SetSessionModelRequest,
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_MODEL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    AcpError::from(match e {
                        kameo::error::RemoteSendError::HandlerError(err) => err,
                        kameo::error::RemoteSendError::ReplyTimeout => AgentError::SessionTimeout {
                            details: "SetSessionModel timed out on remote session".to_string(),
                        },
                        other => AgentError::RemoteActor(other.to_string()),
                    })
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_HISTORY_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    AcpError::from(Self::map_agent_timeout_remote_send_error(
                        e,
                        "GetHistory timed out on remote session",
                    ))
                }),
        }
    }

    /// Get the full durable event stream for this session.
    ///
    /// Returns all persisted `AgentEvent` entries, ordered by sequence number.
    /// Used to replay remote session history on first attach.
    #[tracing::instrument(
        name = "remote.session_ref.get_event_stream",
        skip(self),
        fields(is_remote = self.is_remote(), peer_label = %self.node_label())
    )]
    pub async fn get_event_stream(&self) -> Result<Vec<crate::events::AgentEvent>, AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetEventStream)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetEventStream)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_HISTORY_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    AcpError::from(Self::map_agent_timeout_remote_send_error(
                        e,
                        "GetEventStream timed out on remote session",
                    ))
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    AcpError::from(Self::map_agent_timeout_remote_send_error(
                        e,
                        "GetLlmConfig timed out on remote session",
                    ))
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_infallible_timeout_remote_send_error(
                        e,
                        "GetSessionLimits timed out on remote session",
                    )
                }),
        }
    }

    /// Query current runtime status for stop orchestration.
    pub async fn get_runtime_status(&self) -> Result<messages::SessionRuntimeStatus, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetRuntimeStatus)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetRuntimeStatus)
                .await
                .map_err(Self::map_infallible_remote_send_error),
        }
    }

    /// Query current runtime status with bounded mailbox and reply timeouts.
    pub async fn get_runtime_status_with_timeout(
        &self,
        mailbox_timeout: Duration,
        reply_timeout: Duration,
    ) -> Result<messages::SessionRuntimeStatus, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetRuntimeStatus)
                .mailbox_timeout(mailbox_timeout)
                .reply_timeout(reply_timeout)
                .send()
                .await
                .map_err(|e| match e {
                    kameo::error::SendError::HandlerError(err) => match err {},
                    kameo::error::SendError::Timeout(_) => AgentError::SessionTimeout {
                        details: format!(
                            "GetRuntimeStatus timed out (mailbox={}ms, reply={}ms)",
                            mailbox_timeout.as_millis(),
                            reply_timeout.as_millis()
                        ),
                    },
                    other => AgentError::RemoteActor(other.to_string()),
                }),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetRuntimeStatus)
                .mailbox_timeout(mailbox_timeout)
                .reply_timeout(reply_timeout)
                .send()
                .await
                .map_err(|e| {
                    Self::map_infallible_timeout_remote_send_error(
                        e,
                        format!(
                            "GetRuntimeStatus timed out (mailbox={}ms, reply={}ms)",
                            mailbox_timeout.as_millis(),
                            reply_timeout.as_millis()
                        ),
                    )
                }),
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
            Self::Local(actor_ref) => {
                actor_ref
                    .tell(messages::Shutdown)
                    .await
                    .map_err(|e| AgentError::RemoteActor(e.to_string()))?;
                actor_ref
                    .wait_for_shutdown_result()
                    .await
                    .map(|_| ())
                    .map_err(|e| AgentError::RemoteActor(e.to_string()))
            }

            #[cfg(feature = "remote")]
            Self::Remote { .. } => {
                log::warn!("shutdown called on remote SessionActorRef — not yet supported");
                Ok(())
            }
        }
    }

    /// Shutdown this session actor gracefully with a bounded wait for local shutdown.
    pub async fn shutdown_with_timeout(
        &self,
        mailbox_timeout: Duration,
        reply_timeout: Duration,
    ) -> Result<(), AgentError> {
        match self {
            Self::Local(actor_ref) => {
                actor_ref
                    .tell(messages::Shutdown)
                    .mailbox_timeout(mailbox_timeout)
                    .send()
                    .await
                    .map_err(|e| AgentError::RemoteActor(e.to_string()))?;
                tokio::time::timeout(reply_timeout, actor_ref.wait_for_shutdown_result())
                    .await
                    .map_err(|_| AgentError::SessionTimeout {
                        details: format!(
                            "Shutdown timed out waiting for actor stop (reply={}ms)",
                            reply_timeout.as_millis()
                        ),
                    })?
                    .map(|_| ())
                    .map_err(|e| AgentError::RemoteActor(e.to_string()))
            }

            #[cfg(feature = "remote")]
            Self::Remote { .. } => {
                log::warn!(
                    "shutdown_with_timeout called on remote SessionActorRef — not yet supported"
                );
                Ok(())
            }
        }
    }

    /// Subscribe to events from this session (for remote event relay).
    ///
    /// Registers an event forwarder on the session that sends events to the
    /// specified relay actor (identified by its ActorId as u64).
    ///
    /// `relay_dht_name` is the peer-scoped DHT name under which the relay
    /// actor is registered, so the remote `SessionActor` can look up the
    /// correct per-peer relay.
    #[tracing::instrument(
        name = "remote.session_ref.subscribe_events",
        skip(self),
        fields(
            is_remote = self.is_remote(),
            peer_label = %self.node_label(),
            relay_actor_id,
            relay_dht_name = %relay_dht_name,
        )
    )]
    pub async fn subscribe_events(
        &self,
        relay_actor_id: u64,
        relay_dht_name: String,
    ) -> Result<(), AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::SubscribeEvents {
                    relay_actor_id,
                    relay_dht_name,
                })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::SubscribeEvents {
                    relay_actor_id,
                    relay_dht_name,
                })
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    AcpError::from(Self::map_agent_timeout_remote_send_error(
                        e,
                        "SubscribeEvents timed out on remote session",
                    ))
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    AcpError::from(Self::map_agent_timeout_remote_send_error(
                        e,
                        "SetPlanningContext timed out on remote session",
                    ))
                })?,
        }
        Ok(())
    }

    /// Unsubscribe from events (remove event forwarder).
    pub async fn unsubscribe_events(
        &self,
        relay_actor_id: u64,
        relay_dht_name: String,
    ) -> Result<(), AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::UnsubscribeEvents {
                    relay_actor_id,
                    relay_dht_name,
                })
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::UnsubscribeEvents {
                    relay_actor_id,
                    relay_dht_name,
                })
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    AcpError::from(Self::map_agent_timeout_remote_send_error(
                        e,
                        "UnsubscribeEvents timed out on remote session",
                    ))
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_IO_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| match e {
                    kameo::error::RemoteSendError::HandlerError(err) => err,
                    kameo::error::RemoteSendError::ReplyTimeout => FileProxyError::ActorSend(
                        "GetFileIndex timed out on remote session".to_string(),
                    ),
                    other => FileProxyError::ActorSend(other.to_string()),
                }),
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
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_IO_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| match e {
                    kameo::error::RemoteSendError::HandlerError(err) => err,
                    kameo::error::RemoteSendError::ReplyTimeout => FileProxyError::ActorSend(
                        "ReadRemoteFile timed out on remote session".to_string(),
                    ),
                    other => FileProxyError::ActorSend(other.to_string()),
                }),
        }
    }

    /// Send a scheduled prompt to this session (local only).
    ///
    /// Scheduled execution is always local to the scheduler leader node.
    /// Remote sessions cannot receive `ScheduledPrompt` messages.
    pub async fn tell_scheduled_prompt(
        &self,
        msg: messages::ScheduledPrompt,
    ) -> Result<(), AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .tell(msg)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { .. } => {
                log::warn!(
                    "tell_scheduled_prompt called on remote SessionActorRef — not supported"
                );
                Err(AgentError::Internal(
                    "ScheduledPrompt cannot be sent to remote sessions".to_string(),
                ))
            }
        }
    }
}
