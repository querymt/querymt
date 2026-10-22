//! `SessionActorRef` — location-transparent reference to a `SessionActor`.
//!
//! Every place that currently holds `ActorRef<SessionActor>` switches to
//! `SessionActorRef`. For the `Local` variant, each method delegates to
//! `actor_ref.ask(msg).await`. For the `Remote` variant (behind the `remote`
//! feature), each method delegates to `remote_ref.ask(&msg).await`.

use crate::acp::protocol::{Error as AcpError, PromptRequest, PromptResponse};
use crate::agent::core::AgentMode;
use crate::agent::file_proxy::{FileProxyError, GetFileIndexResponse, ReadRemoteFileResponse};
use crate::agent::messages;
use crate::agent::session_actor::SessionActor;
use crate::agent::session_control::{
    SessionControlState, SessionControlTransition, SessionModelSelection,
};
use crate::agent::undo::{RedoResult, UndoError, UndoResult};
use crate::error::AgentError;
use crate::events::SessionLimits;
use crate::model::AgentMessage;
use crate::session::store::LLMConfig;
use kameo::actor::ActorRef;
use querymt::chat::ReasoningEffort;
use querymt_remote::{
    classify_infallible_remote_send_error, classify_remote_send_error_with_timeout_message,
};
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

/// Map a remote send failure of an interaction message.
///
/// kameo answers `UnknownMessage` when the remote actor has no handler
/// registered for the message ID, i.e. the node runs an agent without the
/// session-interaction contract. Surface that as an explicit upgrade-required
/// error instead of any fallback.
#[cfg(feature = "remote")]
fn map_interaction_send_error(
    node: &str,
    error: kameo::error::RemoteSendError<AgentError>,
    timeout_message: &str,
) -> AgentError {
    if matches!(error, kameo::error::RemoteSendError::UnknownMessage { .. }) {
        return AgentError::RemoteNodeUpgradeRequired {
            node: node.to_string(),
        };
    }
    SessionActorRef::map_agent_timeout_remote_send_error(error, timeout_message)
}

#[cfg(all(test, feature = "remote"))]
mod interaction_error_tests {
    use super::*;
    use kameo::error::RemoteSendError;
    use std::borrow::Cow;

    #[test]
    fn unregistered_interaction_message_requires_node_upgrade() {
        let error = map_interaction_send_error(
            "old-node",
            RemoteSendError::UnknownMessage {
                actor_remote_id: Cow::Borrowed("querymt::SessionActor"),
                message_remote_id: Cow::Borrowed("querymt::ResolveInteraction"),
            },
            "timeout",
        );
        assert!(
            matches!(&error, AgentError::RemoteNodeUpgradeRequired { node } if node == "old-node"),
            "unexpected error: {error:?}"
        );
        let acp: AcpError = error.into();
        assert!(acp.message.contains("must be upgraded"));
    }

    #[test]
    fn other_send_failures_stay_transport_or_handler_errors() {
        let handler = map_interaction_send_error(
            "n",
            RemoteSendError::HandlerError(AgentError::Internal("boom".into())),
            "timeout",
        );
        assert!(matches!(handler, AgentError::Internal(_)));
        let timeout = map_interaction_send_error("n", RemoteSendError::ReplyTimeout, "timed out");
        assert!(timeout.transport_failure().is_some());
    }
}

#[cfg(any(feature = "remote", test))]
async fn collect_event_stream_pages<F, Fut>(
    mut fetch: F,
) -> Result<Vec<crate::events::AgentEvent>, AgentError>
where
    F: FnMut(i64) -> Fut,
    Fut: std::future::Future<Output = Result<messages::EventStreamPage, AgentError>>,
{
    let mut events = Vec::new();
    let mut after = 0;
    let mut boundary = None;
    let mut session_id = None;
    loop {
        let page = fetch(after).await?;
        // Fix the boundary on the first page, rather than chasing concurrent writes.
        let tip = *boundary.get_or_insert(page.latest_source_seq);
        if tip < 0 || page.latest_source_seq < tip {
            return Err(AgentError::Internal("event stream tip regressed".into()));
        }
        let mut previous = after;
        for event in &page.events {
            let expected_session = session_id.get_or_insert_with(|| event.session_id.clone());
            if event.seq <= previous
                || event.seq > page.latest_source_seq
                || &event.session_id != expected_session
            {
                return Err(AgentError::Internal("invalid event stream page".into()));
            }
            previous = event.seq;
        }
        if previous == after && after < tip {
            return Err(AgentError::Internal(
                "event stream page did not advance".into(),
            ));
        }
        events.extend(page.events.into_iter().filter(|event| event.seq <= tip));
        after = previous;
        if after >= tip {
            return Ok(events);
        }
    }
}

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
// Error handling: remote `RemoteSendError` transport variants are classified into
// `AgentError::RemoteTransport` (see `querymt_remote::classify_remote_send_error`)
// so the operation layer can make safe retry decisions from the failure kind and
// delivery certainty; remote handler errors pass through unchanged. Local
// `SendError` failures are in-process and keep mapping to `AgentError::RemoteActor`.

impl SessionActorRef {
    const REMOTE_CONTROL_MAILBOX_TIMEOUT: Duration = Duration::from_secs(10);
    const REMOTE_CONTROL_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
    const REMOTE_PROMPT_MAILBOX_TIMEOUT: Duration = Duration::from_secs(10);
    const REMOTE_PROMPT_REPLY_TIMEOUT: Duration = Duration::from_secs(600);
    const REMOTE_MODEL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
    const REMOTE_UNDO_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
    const REMOTE_HISTORY_REPLY_TIMEOUT: Duration = Duration::from_secs(60);
    const REMOTE_IO_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

    #[cfg(test)]
    pub(super) fn map_local_prompt_send_error(
        error: kameo::error::SendError<messages::Prompt, AgentError>,
    ) -> AcpError {
        AcpError::from(Self::map_local_agent_send_error(error))
    }

    fn map_local_agent_send_error<M>(error: kameo::error::SendError<M, AgentError>) -> AgentError {
        match error {
            kameo::error::SendError::HandlerError(err) => err,
            other => AgentError::RemoteActor(other.to_string()),
        }
    }

    pub(super) fn map_infallible_remote_send_error(
        error: kameo::error::RemoteSendError<kameo::error::Infallible>,
    ) -> AgentError {
        AgentError::from_transport_failure(classify_infallible_remote_send_error(error))
    }

    pub(super) fn map_agent_timeout_remote_send_error(
        error: kameo::error::RemoteSendError<AgentError>,
        timeout_message: impl Into<String>,
    ) -> AgentError {
        match classify_remote_send_error_with_timeout_message(error, timeout_message) {
            Ok(failure) => AgentError::from_transport_failure(failure),
            // Handler errors are delivered remote-side and pass through unchanged.
            Err(handler_error) => handler_error,
        }
    }

    pub(super) fn map_infallible_timeout_remote_send_error(
        error: kameo::error::RemoteSendError<kameo::error::Infallible>,
        timeout_message: impl Into<String>,
    ) -> AgentError {
        match classify_remote_send_error_with_timeout_message(error, timeout_message) {
            Ok(failure) => AgentError::from_transport_failure(failure),
            Err(never) => match never {},
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
    pub async fn prompt_agent(&self, req: PromptRequest) -> Result<PromptResponse, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::Prompt { req })
                .await
                .map_err(Self::map_local_agent_send_error),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => {
                actor_ref
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
                        // Reply loss stays ambiguous (`delivery=unknown`); legacy Prompt
                        // must never be auto-replayed after such a failure.
                        match classify_remote_send_error_with_timeout_message(
                            e,
                            format!(
                                "Remote prompt timed out (mailbox={}s, reply={}s)",
                                Self::REMOTE_PROMPT_MAILBOX_TIMEOUT.as_secs(),
                                Self::REMOTE_PROMPT_REPLY_TIMEOUT.as_secs()
                            ),
                        ) {
                            Ok(failure) => AgentError::from_transport_failure(failure),
                            Err(handler_error) => handler_error,
                        }
                    })
            }
        }
    }

    pub async fn prompt(&self, req: PromptRequest) -> Result<PromptResponse, AcpError> {
        self.prompt_agent(req).await.map_err(AcpError::from)
    }

    pub async fn submit_input(
        &self,
        input: messages::SubmitInput,
    ) -> Result<messages::SubmitInputResult, AgentError> {
        let message = messages::SubmitSessionInput { input };
        match self {
            Self::Local(actor_ref) => actor_ref.ask(message).await.map_err(|error| match error {
                kameo::error::SendError::HandlerError(error) => error,
                other => AgentError::RemoteActor(other.to_string()),
            }),
            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&message)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|error| {
                    Self::map_agent_timeout_remote_send_error(
                        error,
                        "SubmitInput timed out on remote session",
                    )
                }),
        }
    }

    /// Map a remote send failure for the interaction messages. A node that
    /// does not register these messages answers `UnknownMessage`; that means
    /// the node runs an older agent and must be upgraded.
    #[cfg(feature = "remote")]
    fn map_interaction_remote_error(
        &self,
        error: kameo::error::RemoteSendError<AgentError>,
        timeout_message: &str,
    ) -> AgentError {
        map_interaction_send_error(self.node_label(), error, timeout_message)
    }

    /// Snapshot this session's pending interactions (see
    /// [`messages::ListPendingInteractions`]).
    pub async fn list_pending_interactions(
        &self,
        assign_authority: Option<String>,
    ) -> Result<Vec<crate::elicitation::PendingElicitationSnapshot>, AgentError> {
        let message = messages::ListPendingInteractions { assign_authority };
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(message)
                .await
                .map_err(Self::map_local_agent_send_error),
            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&message)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|error| {
                    self.map_interaction_remote_error(
                        error,
                        "ListPendingInteractions timed out on remote session",
                    )
                }),
        }
    }

    /// Claim delivery of pending interactions for a connection (see
    /// [`messages::ClaimInteractionDelivery`]).
    pub async fn claim_interaction_delivery(
        &self,
        interaction_id: Option<String>,
        connection_id: String,
        authority: Option<String>,
    ) -> Result<Vec<crate::elicitation::ClaimedElicitationDelivery>, AgentError> {
        let message = messages::ClaimInteractionDelivery {
            interaction_id,
            connection_id,
            authority,
        };
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(message)
                .await
                .map_err(Self::map_local_agent_send_error),
            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&message)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|error| {
                    self.map_interaction_remote_error(
                        error,
                        "ClaimInteractionDelivery timed out on remote session",
                    )
                }),
        }
    }

    /// Resolve a pending interaction (see [`messages::ResolveInteraction`]).
    pub async fn resolve_interaction(
        &self,
        interaction_id: String,
        connection_id: String,
        delivery_generation: Option<u64>,
        response: crate::elicitation::ElicitationResponse,
    ) -> Result<crate::elicitation::ElicitationResolution, AgentError> {
        let message = messages::ResolveInteraction {
            interaction_id,
            connection_id,
            delivery_generation,
            response,
        };
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(message)
                .await
                .map_err(Self::map_local_agent_send_error),
            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&message)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|error| {
                    self.map_interaction_remote_error(
                        error,
                        "ResolveInteraction timed out on remote session",
                    )
                }),
        }
    }

    pub async fn discard_queued_input(
        &self,
        input_id: String,
    ) -> Result<messages::DiscardQueuedInputResult, AgentError> {
        let message = messages::DiscardQueuedInput { input_id };
        match self {
            Self::Local(actor_ref) => actor_ref.ask(message).await.map_err(|error| match error {
                kameo::error::SendError::HandlerError(error) => error,
                other => AgentError::RemoteActor(other.to_string()),
            }),
            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&message)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|error| {
                    Self::map_agent_timeout_remote_send_error(
                        error,
                        "DiscardQueuedInput timed out on remote session",
                    )
                }),
        }
    }

    pub async fn steer(
        &self,
        session_id: String,
        expected_run_id: String,
        prompt: Vec<crate::acp::protocol::ContentBlock>,
    ) -> Result<messages::SubmitInputResult, AgentError> {
        self.submit_input(messages::SubmitInput {
            session_id,
            client_input_id: None,
            expected_run_id: Some(expected_run_id),
            delivery: messages::InputDelivery::Steer,
            prompt,
        })
        .await
    }

    pub async fn queue(
        &self,
        session_id: String,
        prompt: Vec<crate::acp::protocol::ContentBlock>,
    ) -> Result<messages::SubmitInputResult, AgentError> {
        self.submit_input(messages::SubmitInput {
            session_id,
            client_input_id: None,
            expected_run_id: None,
            delivery: messages::InputDelivery::Queue,
            prompt,
        })
        .await
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
                .map_err(Self::map_infallible_remote_send_error),
        }
    }

    /// Set the agent mode for this session.
    pub async fn set_mode(&self, mode: AgentMode) -> Result<SessionControlTransition, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::SetMode { mode })
                .await
                .map_err(Self::map_local_agent_send_error),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::SetMode { mode })
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_MODEL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_agent_timeout_remote_send_error(
                        e,
                        "SetMode timed out on remote session",
                    )
                }),
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
    ) -> Result<SessionControlTransition, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::SetReasoningEffort { effort })
                .await
                .map_err(Self::map_local_agent_send_error),

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
                }),
        }
    }

    /// Get the complete authoritative session control state.
    pub async fn get_session_control(&self) -> Result<SessionControlState, AgentError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(messages::GetSessionControl)
                .await
                .map_err(Self::map_local_agent_send_error),
            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&messages::GetSessionControl)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_CONTROL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_agent_timeout_remote_send_error(
                        e,
                        "GetSessionControl timed out on remote session",
                    )
                }),
        }
    }

    /// Apply a complete LLM configuration through the same atomic control path.
    pub async fn set_llm_config(
        &self,
        config: querymt::LLMParams,
        provider_node_id: Option<String>,
    ) -> Result<(), AgentError> {
        let msg = messages::SetLlmConfig {
            config,
            provider_node_id,
        };
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(msg)
                .await
                .map_err(Self::map_local_agent_send_error),
            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&msg)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_MODEL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|error| {
                    Self::map_agent_timeout_remote_send_error(
                        error,
                        "SetLlmConfig timed out on remote session",
                    )
                }),
        }
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
                .map_err(|e| {
                    match classify_remote_send_error_with_timeout_message(
                        e,
                        "Undo timed out on remote session",
                    ) {
                        Ok(failure) => UndoError::from_transport_failure(failure),
                        Err(handler_error) => handler_error,
                    }
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
                .map_err(|e| {
                    match classify_remote_send_error_with_timeout_message(
                        e,
                        "Redo timed out on remote session",
                    ) {
                        Ok(failure) => UndoError::from_transport_failure(failure),
                        Err(handler_error) => handler_error,
                    }
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

    /// Set the effective model and remember it for the current session mode.
    pub async fn set_session_model(
        &self,
        selection: SessionModelSelection,
    ) -> Result<SessionControlTransition, AgentError> {
        let msg = messages::SetSessionModel { selection };
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(msg)
                .await
                .map_err(Self::map_local_agent_send_error),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&msg)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_MODEL_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_agent_timeout_remote_send_error(
                        e,
                        "SetSessionModel timed out on remote session",
                    )
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
            Self::Remote { .. } => collect_event_stream_pages(|after| {
                self.get_event_stream_since(Some(after), messages::MAX_EVENT_STREAM_PAGE_SIZE)
            })
            .await
            .map_err(AcpError::from),
        }
    }

    /// Get one bounded page of the durable event stream after a source-side
    /// cursor (plan §16).
    ///
    /// Transport failures are typed (`AgentError::RemoteTransport`) so backfill
    /// can distinguish unsupported mixed-version hosts (e.g. unknown message)
    /// from transient errors without string matching.
    #[tracing::instrument(
        name = "remote.session_ref.get_event_stream_since",
        skip(self),
        fields(is_remote = self.is_remote(), peer_label = %self.node_label())
    )]
    pub async fn get_event_stream_since(
        &self,
        after_source_seq: Option<i64>,
        limit: usize,
    ) -> Result<messages::EventStreamPage, AgentError> {
        let message = messages::GetEventStreamSince {
            after_source_seq,
            limit,
        };
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(message)
                .await
                .map_err(|e| AgentError::RemoteActor(e.to_string())),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&message)
                .mailbox_timeout(Self::REMOTE_CONTROL_MAILBOX_TIMEOUT)
                .reply_timeout(Self::REMOTE_HISTORY_REPLY_TIMEOUT)
                .send()
                .await
                .map_err(|e| {
                    Self::map_agent_timeout_remote_send_error(
                        e,
                        "GetEventStreamSince timed out on remote session",
                    )
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
        self.subscribe_events_message(messages::SubscribeEvents {
            relay_actor_id,
            #[cfg(feature = "remote")]
            relay_ref: None,
            relay_dht_name: Some(relay_dht_name),
            after_source_seq: None,
        })
        .await
    }

    #[cfg(feature = "remote")]
    pub async fn subscribe_events_direct(
        &self,
        relay_actor_id: u64,
        relay_ref: kameo::actor::RemoteActorRef<crate::agent::remote::EventRelayActor>,
        relay_dht_name: String,
    ) -> Result<(), AcpError> {
        self.subscribe_events_message(messages::SubscribeEvents {
            relay_actor_id,
            relay_ref: Some(relay_ref),
            relay_dht_name: Some(relay_dht_name),
            after_source_seq: None,
        })
        .await
    }

    async fn subscribe_events_message(
        &self,
        message: messages::SubscribeEvents,
    ) -> Result<(), AcpError> {
        match self {
            Self::Local(actor_ref) => actor_ref
                .ask(message)
                .await
                .map_err(|e| AcpError::from(AgentError::RemoteActor(e.to_string()))),

            #[cfg(feature = "remote")]
            Self::Remote { actor_ref, .. } => actor_ref
                .ask(&message)
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
                .map_err(|e| {
                    match classify_remote_send_error_with_timeout_message(
                        e,
                        "GetFileIndex timed out on remote session",
                    ) {
                        Ok(failure) => FileProxyError::from_transport_failure(failure),
                        Err(handler_error) => handler_error,
                    }
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
                .map_err(|e| {
                    match classify_remote_send_error_with_timeout_message(
                        e,
                        "ReadRemoteFile timed out on remote session",
                    ) {
                        Ok(failure) => FileProxyError::from_transport_failure(failure),
                        Err(handler_error) => handler_error,
                    }
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

#[cfg(test)]
mod history_paging_tests {
    use super::*;

    fn page(seqs: impl IntoIterator<Item = i64>, tip: i64) -> messages::EventStreamPage {
        messages::EventStreamPage {
            events: seqs
                .into_iter()
                .map(|seq| crate::events::AgentEvent {
                    session_seq: 0,
                    seq,
                    timestamp: 0,
                    session_id: "s1".into(),
                    origin: crate::events::EventOrigin::Local,
                    source_node: None,
                    kind: crate::events::AgentEventKind::Cancelled,
                })
                .collect(),
            latest_source_seq: tip,
        }
    }

    #[tokio::test]
    async fn collects_all_pages_including_histories_beyond_old_cap() {
        for total in [0, 200, 201, 10_001] {
            let mut calls = 0;
            let events = collect_event_stream_pages(|after| {
                calls += 1;
                let end = (after + messages::MAX_EVENT_STREAM_PAGE_SIZE as i64).min(total);
                std::future::ready(Ok(page(after + 1..=end, total)))
            })
            .await
            .unwrap();
            assert_eq!(events.len(), total as usize);
            assert_eq!(
                calls,
                ((total as usize).div_ceil(messages::MAX_EVENT_STREAM_PAGE_SIZE)).max(1)
            );
            assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
        }
    }

    #[tokio::test]
    async fn stops_at_initial_tip_even_when_new_events_arrive() {
        let mut pages = vec![page([1, 3], 5), page([5, 7], 7)].into_iter();
        let events = collect_event_stream_pages(|_| std::future::ready(Ok(pages.next().unwrap())))
            .await
            .unwrap();
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 3, 5]
        );
    }

    #[tokio::test]
    async fn rejects_invalid_pages_and_propagates_midstream_errors() {
        let mut wrong_session = page([3], 3);
        wrong_session.events[0].session_id = "other".into();
        for invalid in [
            page([], 3),
            page([2], 3),
            page([3, 3], 3),
            page([4], 3),
            page([3], 2),
            wrong_session,
        ] {
            let mut pages = vec![page([1, 2], 3), invalid].into_iter();
            assert!(
                collect_event_stream_pages(|_| std::future::ready(Ok(pages.next().unwrap())))
                    .await
                    .is_err()
            );
        }
        let mut calls = 0;
        let result = collect_event_stream_pages(|_| {
            calls += 1;
            std::future::ready(if calls == 1 {
                Ok(page([1], 2))
            } else {
                Err(AgentError::Internal("page read failed".into()))
            })
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("page read failed"));
    }
}
