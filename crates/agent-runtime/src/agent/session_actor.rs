//! Per-session actor. Each session gets its own actor with isolated state.
//!
//! Prompt execution uses `ctx.spawn()` so the actor stays responsive for
//! `Cancel`, `SetMode`, and other messages between turns.

use crate::acp::client_bridge::ClientBridgeSender;
use crate::acp::protocol::{ExtResponse, PromptResponse, StopReason};
use crate::agent::agent_config::AgentConfig;
use crate::agent::core::{AgentMode, SessionRuntime, ToolConfig, TurnSnapshot};
use crate::agent::execution::turn::{ExecutionOrigin, PreTurnSnapshot, Turn, TurnEnv};
use crate::agent::execution::{CycleOutcome, TurnError};
use crate::agent::messages::*;
use crate::agent::session_control::{
    SessionControlState, SessionControlTransition, SessionModelBinding,
};
use crate::agent::turn_control::{
    ActiveRun, InputDelivery, RunPhase, RunSignal, SubmitInputResult, TurnControlError, now_ms,
};
use crate::agent::undo::{RedoResult, UndoError, UndoResult};
use crate::agent::utils::{format_prompt_user_text_only, genai, render_prompt_for_display};
use crate::error::AgentError;
use crate::hooks::HookNotice;
use crate::session::provider::ResolvedModel;
use crate::session::run_lease::{RunLease, RunLeaseRoot};
use querymt_agent_core::events::{AgentEventKind, RunOutcome, SessionLimits};
use querymt_agent_core::model::{
    AgentMessage, MessagePart, prompt_contains_images, validate_prompt_blocks,
};

/// How long a run start waits for a momentarily held run lease (a reconciler
/// or session delete) before treating the session as running elsewhere.
const RUN_LEASE_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
use crate::session::runtime::RuntimeContext;
use crate::session::store::LLMConfig;
use crate::slash_commands::builtin::{Builtin, Submission};
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::reply::DelegatedReply;
use log::{debug, error, info, warn};
use querymt::chat::{ChatRole, ReasoningEffort};
#[cfg(test)]
use querymt::error::LLMError;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, info_span, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

fn client_prompt_id_from_meta(meta: Option<&crate::acp::protocol::Meta>) -> Option<String> {
    meta.and_then(|meta| meta.get("querymt"))
        .and_then(serde_json::Value::as_object)
        .and_then(|querymt| querymt.get("client_prompt_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_transport_model_id(model_id: &str, fallback_provider: &str) -> (String, String) {
    if let Some(slash_pos) = model_id.find('/') {
        (
            model_id[..slash_pos].to_string(),
            model_id[slash_pos + 1..].to_string(),
        )
    } else {
        // Backward compatibility for legacy clients that still send a bare model id.
        (fallback_provider.to_string(), model_id.to_string())
    }
}

enum ControlTransitionIntent {
    Mode(AgentMode),
    Model(crate::agent::session_control::SessionModelSelection),
    ModelConfig {
        model_id: String,
        provider_node_id: Option<String>,
        params: Box<querymt::LLMParams>,
    },
    ReasoningEffort(Option<ReasoningEffort>),
}

const ATTACHMENTS_PLACEHOLDER: &str = "(attachments included)";

fn emit_hook_notices(config: &AgentConfig, session_id: &str, notices: Vec<HookNotice>) {
    for notice in notices {
        config.emit_event(
            session_id,
            AgentEventKind::HookNotice {
                event_name: notice.event_name,
                message: notice.message,
                is_error: notice.is_error,
            },
        );
    }
}

fn summarize_intent_candidate(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed == ATTACHMENTS_PLACEHOLDER {
        return None;
    }

    let single_line = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.is_empty() {
        return None;
    }

    Some(querymt_utils::str_utils::truncate_with_ellipsis(
        &single_line,
        240,
    ))
}

async fn maybe_update_session_intent(
    _config: &AgentConfig,
    _session_id: &str,
    turn: &mut Turn,
    user_text: &str,
    source_ref: &str,
) -> Result<(), AgentError> {
    let Some(candidate) = summarize_intent_candidate(user_text) else {
        return Ok(());
    };
    if turn.state.current_intent.as_ref().is_some_and(|intent| {
        intent.summary == candidate || intent.next_step_hint.as_deref() == Some(candidate.as_str())
    }) {
        return Ok(());
    }

    let summary = turn
        .state
        .current_intent
        .as_ref()
        .map(|intent| intent.summary.clone())
        .unwrap_or_else(|| candidate.clone());
    let constraints = turn
        .state
        .current_intent
        .as_ref()
        .and_then(|intent| intent.constraints.clone());
    let next_step_hint = turn.state.current_intent.as_ref().map(|_| candidate);
    turn.state
        .update_intent_projection(
            summary,
            constraints,
            next_step_hint,
            "user_prompt".to_string(),
            Some(source_ref.to_string()),
        )
        .await
        .map_err(|e| AgentError::Internal(e.to_string()))?;
    Ok(())
}

/// Per-session actor. Each session gets its own actor with isolated state.
pub struct SessionActor {
    // ── Shared config (read at turn start) ───────────────────────
    pub(crate) config: Arc<AgentConfig>,

    // ── Session identity ─────────────────────────────────────────
    pub(crate) session_id: String,

    // ── Session state (OWNED, NO LOCKS) ──────────────────────────
    pub(crate) runtime: Arc<SessionRuntime>,
    pub(crate) mode: AgentMode,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) control_state: Option<SessionControlState>,
    pub(crate) tool_config: ToolConfig,

    // ── Run tracking ─────────────────────────────────────────────
    /// The run this session executes, if any, with its cancellation token and authority.
    /// Only [`SessionActor::start_run`] fills it and `PromptFinished` empties it.
    pub(crate) run: RunSlot,
    /// Advances when a run starts, so a stale `PromptFinished` of an earlier
    /// run is ignored.
    pub(crate) generation: u64,
    runtime_epoch: String,
    runtime_revision: u64,
    published_pending_steering: usize,
    /// Cross-process run lease, held from before `RunStarted` until the last
    /// queued run's `RunCompleted` is persisted. Kept outside `ActiveRun` so
    /// no clone of run state can extend it, and so it can be handed over to
    /// the next queued prompt without a gap.
    run_lease: Option<RunLease>,
    queued_prompts: VecDeque<QueuedPrompt>,
    submit_receipts: VecDeque<SubmitInputReceipt>,

    // ── Mesh (remote sessions only) ──────────────────────────────
    /// Present when this actor was spawned on a mesh node via
    /// `RemoteNodeManager`. Used by `SubscribeEvents` to look up the
    /// remote `EventRelayActor` in the Kademlia DHT.
    #[cfg(feature = "remote")]
    pub(crate) mesh: Option<crate::agent::remote::MeshHandle>,

    // Tracks owned EventForwarder tasks by relay actor so replacement, unsubscribe,
    // and SessionActor teardown cancel the corresponding background task.
    pub(crate) relay_forwarder_handles: HashMap<
        kameo::actor::ActorId,
        (
            Option<String>,
            crate::agent::remote::event_forwarder::EventForwarderHandle,
        ),
    >,
}

impl Actor for SessionActor {
    type Args = Self;
    type Error = kameo::error::Infallible;

    fn name() -> &'static str {
        "SessionActor"
    }

    async fn on_start(
        args: Self::Args,
        _actor_ref: kameo::actor::ActorRef<Self>,
    ) -> Result<Self, Self::Error> {
        Ok(args)
    }

    async fn on_link_died(
        &mut self,
        _actor_ref: kameo::actor::WeakActorRef<Self>,
        id: kameo::actor::ActorId,
        reason: kameo::error::ActorStopReason,
    ) -> Result<std::ops::ControlFlow<kameo::error::ActorStopReason>, Self::Error> {
        use kameo::error::ActorStopReason;
        use std::ops::ControlFlow;
        // An event subscriber observes the session; it does not own its lifetime.
        if self.relay_forwarder_handles.remove(&id).is_some() {
            return Ok(ControlFlow::Continue(()));
        }
        Ok(match reason {
            ActorStopReason::Normal | ActorStopReason::SupervisorRestart => {
                ControlFlow::Continue(())
            }
            #[cfg(feature = "remote")]
            ActorStopReason::PeerDisconnected => {
                ControlFlow::Break(ActorStopReason::PeerDisconnected)
            }
            reason => ControlFlow::Break(ActorStopReason::LinkDied {
                id,
                reason: Box::new(reason),
            }),
        })
    }

    /// Release the session's ordered event stream once everything emitted so
    /// far is persisted, so stopped sessions do not keep a writer task alive.
    async fn on_stop(
        &mut self,
        _actor_ref: kameo::actor::WeakActorRef<Self>,
        reason: kameo::error::ActorStopReason,
    ) -> Result<(), Self::Error> {
        match &reason {
            kameo::error::ActorStopReason::Normal
            | kameo::error::ActorStopReason::SupervisorRestart => {
                debug!("SessionActor {} stopped: {}", self.session_id, reason);
            }
            _ => warn!("SessionActor {} stopped: {}", self.session_id, reason),
        }
        // A stopped actor must not leave work behind. Cancelling the current
        // token ends a run that is still winding down, and dropping the queued
        // prompts keeps a finished run from starting the next one. Releasing
        // the lease here (kameo reports shutdown complete before the actor
        // itself is dropped) means it is free once `shutdown()` returns; an
        // unmatched `run_started` is then closed by reconciliation.
        self.run.cancel();
        for queued in self.queued_prompts.drain(..) {
            let _ = queued
                .reply
                .send(Ok(PromptResponse::new(StopReason::Cancelled)));
        }
        self.run_lease = None;
        self.config
            .event_sink
            .retire_session_stream(&self.session_id)
            .await;
        Ok(())
    }
}

/// Whether the session is executing a run. The cancellation token belongs to
/// the run: an idle session has nothing to cancel.
#[derive(Debug)]
pub(crate) enum RunSlot {
    Idle,
    Running(ActiveRun, TurnAuthority),
}

impl RunSlot {
    /// The running run.
    pub(crate) fn active(&self) -> Option<&ActiveRun> {
        match self {
            RunSlot::Idle => None,
            RunSlot::Running(run, _) => Some(run),
        }
    }

    /// The running run, for updating its phase.
    pub(crate) fn active_mut(&mut self) -> Option<&mut ActiveRun> {
        match self {
            RunSlot::Idle => None,
            RunSlot::Running(run, _) => Some(run),
        }
    }

    fn authority(&self) -> Option<&TurnAuthority> {
        match self {
            RunSlot::Idle => None,
            RunSlot::Running(_, authority) => Some(authority),
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        matches!(self, RunSlot::Idle)
    }

    /// Cancel the running run; nothing happens when idle.
    pub(crate) fn cancel(&self) {
        if let RunSlot::Running(run, _) = self {
            run.cancellation.cancel();
        }
    }

    /// Whether cancellation was requested for the running run.
    pub(crate) fn cancel_requested(&self) -> bool {
        self.active()
            .is_some_and(|run| run.cancellation.is_cancelled())
    }
}

const MAX_QUEUED_PROMPTS: usize = 32;

/// Bounded same-actor retry safety for keyed `SubmitInput`.
///
/// Stores the accepted `SubmitInputResult` keyed by `client_input_id` so a
/// retried submission (same ID, lost reply) returns the original receipt
/// instead of enqueuing a duplicate turn. Same-actor only: a new actor after
/// restart/materialization has an empty cache, and callers must not assume
/// cross-restart exactly-once execution.
const MAX_SUBMIT_RECEIPTS: usize = 128;
const MAX_SUBMIT_RECEIPT_AGE_MS: u64 = 30 * 60 * 1000;

#[derive(Debug, Clone)]
struct SubmitInputReceipt {
    client_input_id: String,
    authority: TurnAuthority,
    result: SubmitInputResult,
    accepted_at_ms: u64,
}

fn queued_prompt_capacity_error(session_id: &str) -> AgentError {
    AgentError::TurnControl {
        kind: "queue_full".to_string(),
        message: format!(
            "session {session_id} queued prompt capacity ({MAX_QUEUED_PROMPTS}) reached"
        ),
    }
}

/// How a started interactive-or-scheduled run hands its result back. How the
/// run started is its [`ExecutionOrigin`], which decides what is emitted when
/// it ends.
struct Completion {
    /// The reply goes out once the run's last event is delivered.
    reply: oneshot::Sender<Result<PromptResponse, AgentError>>,
    /// The queued input this run starts, announced after `RunStarted`.
    queued_input_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(crate) enum TurnAuthority {
    #[default]
    Local,
    #[cfg(feature = "remote")]
    Peer(Arc<PeerTurn>),
}

#[cfg(feature = "remote")]
pub(crate) struct PeerTurn {
    pub(crate) key: querymt_remote::peer_access::PeerGrantKey,
    pub(crate) protocol: querymt_remote::peer_access::SharingProtocol,
    pinned: querymt_remote::peer_access::PeerGrant,
    changes: parking_lot::Mutex<
        Option<tokio::sync::broadcast::Receiver<querymt_remote::peer_access::PolicyChange>>,
    >,
    store: Arc<dyn querymt_remote::peer_access::GrantStore>,
    /// A model pass the submitter signed for this turn. It is checked against
    /// the session's binding when the turn starts, and goes only to the model
    /// node it names.
    pass: Option<querymt_remote::ModelPass>,
}

#[cfg(feature = "remote")]
impl PeerTurn {
    /// Keep the model pass the submitter attached to this turn or queued input.
    pub(crate) fn with_pass(mut self, pass: Option<querymt_remote::ModelPass>) -> Self {
        self.pass = pass;
        self
    }

    pub(crate) fn pass(&self) -> Option<&querymt_remote::ModelPass> {
        self.pass.as_ref()
    }

    pub(crate) fn from_request(
        origin: &querymt_remote::mesh_ingress::PeerRequest,
        store: Arc<dyn querymt_remote::peer_access::GrantStore>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            key: origin.key().clone(),
            protocol: origin.protocol(),
            pinned: origin
                .grant()
                .cloned()
                .ok_or(AgentError::MeshAccessDenied)?,
            changes: parking_lot::Mutex::new(store.subscribe_changes()),
            store,
            pass: None,
        })
    }

    pub(crate) async fn grant(&self) -> Result<querymt_remote::peer_access::PeerGrant, AgentError> {
        self.grant_for(None).await
    }

    pub(crate) async fn grant_for(
        &self,
        binding: Option<(&str, &str)>,
    ) -> Result<querymt_remote::peer_access::PeerGrant, AgentError> {
        let state = self
            .store
            .grant(&self.key)
            .await
            .map_err(|_| AgentError::MeshAccessDenied)?;
        self.check_grant(&state.grant, binding)?;
        Ok(self.pinned.clone())
    }

    /// Whether `node` is the prompting peer's own node. A model there runs on
    /// the peer's resources, under the grant that node gives this host, so
    /// this host's model grant and quotas do not govern it.
    pub(crate) fn is_peer_node(&self, node: Option<&str>) -> bool {
        node.is_some_and(|node| {
            querymt_remote::NodeId::parse(node).is_ok_and(|node| node == self.key.peer)
        })
    }

    /// The `(provider, model)` this host's grant must allow for a turn on
    /// `session`: none when the model runs on the peer's own node.
    pub(crate) fn host_binding<'a>(
        &self,
        session: &'a crate::session::provider::SessionHandle,
    ) -> Option<(&'a str, &'a str)> {
        // A model on the peer's node, or one the peer lends through a pass, is
        // governed by that node's grant for the peer, not by this host's model
        // grant. `SessionHandle::for_mesh_turn` refuses a pass that does not
        // match the session's binding.
        if self.is_peer_node(session.provider_node_id()) || self.pass.is_some() {
            return None;
        }
        session
            .llm_config()
            .map(|config| (config.provider.as_str(), config.model.as_str()))
    }

    pub(crate) fn allows_shell(&self) -> bool {
        self.grant_allows_shell(&self.pinned)
    }

    fn grant_allows_shell(&self, grant: &querymt_remote::peer_access::PeerGrant) -> bool {
        grant.allows_shell(self.protocol)
    }

    fn check_grant(
        &self,
        grant: &querymt_remote::peer_access::PeerGrant,
        binding: Option<(&str, &str)>,
    ) -> Result<(), AgentError> {
        use querymt_remote::peer_access::PeerGrant;

        if !grant.allows_sessions(self.protocol)
            // Even a Custom grant with shell does not authorize a Full turn's other tools.
            || self.pinned == PeerGrant::Full && *grant != PeerGrant::Full
            || self.allows_shell() && !self.grant_allows_shell(grant)
            || self.pinned.allows_filesystem(self.protocol)
                && !grant.allows_filesystem(self.protocol)
            || binding.is_some_and(|(provider, model)| {
                !self.pinned.allows_model(self.protocol, provider, model)
                    || !grant.allows_model(self.protocol, provider, model)
            })
        {
            return Err(AgentError::MeshAccessDenied);
        }
        Ok(())
    }

    pub(crate) async fn revoked(&self, binding: Option<(&str, &str)>) -> AgentError {
        let changes = self
            .changes
            .lock()
            .take()
            .or_else(|| self.store.subscribe_changes());
        querymt_remote::peer_access::wait_for_policy_loss(
            self.store.as_ref(),
            &self.key,
            changes,
            |grant| self.check_grant(grant, binding),
            || AgentError::MeshAccessDenied,
        )
        .await
    }

    /// Check a one-shot mutation at the last boundary before starting its side effect.
    pub(crate) async fn check_mutation(&self) -> Result<(), AgentError> {
        self.check_mutation_grants(
            |grant| self.check_grant(grant, None),
            |_| AgentError::MeshAccessDenied,
        )
        .await
    }

    pub(crate) async fn check_schedule_mutation(&self) -> Result<(), AgentError> {
        self.check_mutation_grants(
            |grant| {
                if grant.allows_sessions(self.protocol) {
                    Ok(())
                } else {
                    Err(AgentError::MeshAccessDenied)
                }
            },
            |_| AgentError::Internal("mesh access store unavailable".into()),
        )
        .await
    }

    async fn check_mutation_grants(
        &self,
        check: impl Fn(&querymt_remote::peer_access::PeerGrant) -> Result<(), AgentError>,
        store_error: impl FnOnce(querymt_remote::peer_access::GrantStoreError) -> AgentError,
    ) -> Result<(), AgentError> {
        use querymt_remote::peer_access::{PolicyChange, PolicyEffect};
        use tokio::sync::broadcast::error::TryRecvError;

        let state = self.store.grant(&self.key).await.map_err(store_error)?;
        check(&state.grant)?;
        if let Some(changes) = self.changes.lock().as_mut() {
            loop {
                match changes.try_recv() {
                    Ok(PolicyChange {
                        key,
                        effect: PolicyEffect::Grant(grant),
                    }) if key == self.key => {
                        check(&grant)?;
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(_) => return Err(AgentError::MeshAccessDenied),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn schedule_origin(&self) -> crate::session::domain_schedule::ScheduleOrigin {
        use crate::session::domain_schedule::{ScheduleOrigin, SchedulePeerProtocol};
        use querymt_remote::peer_access::SharingProtocol;
        ScheduleOrigin::Peer {
            host: self.key.host.to_string(),
            peer: self.key.peer.to_string(),
            protocol: match self.protocol {
                SharingProtocol::Legacy => SchedulePeerProtocol::Legacy,
                SharingProtocol::V1 => SchedulePeerProtocol::V1,
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn test_peer(
        key: querymt_remote::peer_access::PeerGrantKey,
        pinned: querymt_remote::peer_access::PeerGrant,
        store: Arc<dyn querymt_remote::peer_access::GrantStore>,
    ) -> Arc<Self> {
        Self::test_peer_with_pass(key, pinned, store, None)
    }

    #[cfg(test)]
    pub(crate) fn test_peer_with_pass(
        key: querymt_remote::peer_access::PeerGrantKey,
        pinned: querymt_remote::peer_access::PeerGrant,
        store: Arc<dyn querymt_remote::peer_access::GrantStore>,
        pass: Option<querymt_remote::ModelPass>,
    ) -> Arc<Self> {
        Arc::new(Self {
            key,
            protocol: querymt_remote::peer_access::SharingProtocol::V1,
            pinned,
            changes: parking_lot::Mutex::new(store.subscribe_changes()),
            store,
            pass,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_authority(
        pinned: querymt_remote::peer_access::PeerGrant,
        store: Arc<dyn querymt_remote::peer_access::GrantStore>,
    ) -> TurnAuthority {
        use querymt_remote::{
            NodeId,
            peer_access::{PeerGrantKey, SharingProtocol},
        };
        TurnAuthority::Peer(Arc::new(Self {
            key: PeerGrantKey {
                host: NodeId::from_peer_id(libp2p::PeerId::random()),
                peer: NodeId::from_peer_id(libp2p::PeerId::random()),
            },
            protocol: SharingProtocol::V1,
            pinned,
            changes: parking_lot::Mutex::new(store.subscribe_changes()),
            store,
            pass: None,
        }))
    }
}

#[cfg(feature = "remote")]
impl std::fmt::Debug for PeerTurn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerTurn")
            .field("key", &self.key)
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl TurnAuthority {
    pub(crate) fn stored_origin(&self) -> crate::session::domain_schedule::ScheduleOrigin {
        match self {
            Self::Local => crate::session::domain_schedule::ScheduleOrigin::Local,
            #[cfg(feature = "remote")]
            Self::Peer(peer) => peer.schedule_origin(),
        }
    }

    #[cfg(feature = "remote")]
    pub(crate) fn owns_schedule(
        &self,
        origin: &crate::session::domain_schedule::ScheduleOrigin,
    ) -> bool {
        use crate::session::domain_schedule::ScheduleOrigin;
        match (self, origin) {
            (Self::Peer(caller), ScheduleOrigin::Peer { host, peer, .. }) => {
                caller.key.host.to_string() == *host && caller.key.peer.to_string() == *peer
            }
            _ => false,
        }
    }

    pub(crate) async fn check(&self) -> Result<(), AgentError> {
        match self {
            Self::Local => Ok(()),
            #[cfg(feature = "remote")]
            Self::Peer(peer) => peer.grant().await.map(|_| ()),
        }
    }

    fn same_origin(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local, Self::Local) => true,
            #[cfg(feature = "remote")]
            (Self::Peer(a), Self::Peer(b)) => a.key == b.key && a.protocol == b.protocol,
            #[cfg(feature = "remote")]
            _ => false,
        }
    }

    fn can_control(&self, target: &Self) -> bool {
        if matches!(self, Self::Local) {
            return true;
        }
        if !self.same_origin(target) {
            return false;
        }
        #[cfg(feature = "remote")]
        if let (Self::Peer(caller), Self::Peer(turn)) = (self, target) {
            return turn.check_grant(&caller.pinned, None).is_ok();
        }
        true
    }
}

struct QueuedPrompt {
    input_id: String,
    authority: TurnAuthority,
    req: crate::acp::protocol::PromptRequest,
    reply: oneshot::Sender<Result<PromptResponse, AgentError>>,
}

impl SessionActor {
    /// Create a new SessionActor. Call `kameo::actor::spawn()` to start it.
    pub fn new(config: Arc<AgentConfig>, session_id: String, runtime: Arc<SessionRuntime>) -> Self {
        let tool_config = config.tool_config.clone();
        // Read current default mode from the shared mutex
        let mode = config
            .default_mode
            .lock()
            .map(|m| *m)
            .unwrap_or(AgentMode::Build);
        // Read current default reasoning effort (lock-free via ArcSwap)
        let reasoning_effort = **config.default_reasoning_effort.load();
        Self {
            config,
            session_id,
            runtime,
            mode,
            reasoning_effort,
            control_state: None,
            tool_config,
            run: RunSlot::Idle,
            generation: 0,
            runtime_epoch: Uuid::new_v4().to_string(),
            runtime_revision: 0,
            published_pending_steering: 0,
            run_lease: None,
            queued_prompts: VecDeque::new(),
            submit_receipts: VecDeque::new(),
            #[cfg(feature = "remote")]
            mesh: None,
            relay_forwarder_handles: HashMap::new(),
        }
    }

    fn runtime_status(&self, pending_steering_count: usize) -> SessionRuntimeStatus {
        let phase = if self.run.cancel_requested() {
            SessionRuntimePhase::CancelRequested
        } else {
            match self.run.active().map(|run| run.phase) {
                None => SessionRuntimePhase::Idle,
                Some(RunPhase::Starting) => SessionRuntimePhase::Starting,
                Some(RunPhase::Model) => SessionRuntimePhase::Model,
                Some(RunPhase::Tools) => SessionRuntimePhase::Tools,
                Some(RunPhase::Waiting) => SessionRuntimePhase::Waiting,
                Some(RunPhase::Closing) => SessionRuntimePhase::Closing,
            }
        };
        SessionRuntimeStatus {
            phase,
            active_run_id: self.run.active().map(|run| run.run_id.to_string()),
            steerable: self.run.active().is_some_and(|run| {
                run.phase != RunPhase::Closing && !run.cancellation.is_cancelled()
            }),
            pending_steering_count: crate::agent::utils::u32_from_usize(
                pending_steering_count,
                "pending_steering_count",
                Some(&self.session_id),
            ),
            queued_input_count: crate::agent::utils::u32_from_usize(
                self.queued_prompts.len(),
                "queued_input_count",
                Some(&self.session_id),
            ),
            run_started_at_ms: self.run.active().map(|run| run.started_at_ms),
            observation: crate::agent::messages::RuntimeObservation {
                availability: crate::agent::messages::RuntimeAvailability::Live,
                epoch: Some(self.runtime_epoch.clone()),
                revision: Some(self.runtime_revision),
                reason: None,
            },
            delegation_activity: self
                .config
                .event_sink
                .live_delegation_activity(&self.session_id),
        }
    }

    fn emit_runtime_changed(&mut self, pending: usize) {
        self.published_pending_steering = pending;
        self.runtime_revision = self.runtime_revision.saturating_add(1);
        self.config.emit_event(
            &self.session_id,
            AgentEventKind::RuntimeChanged {
                runtime: self.runtime_status(pending),
            },
        );
    }

    async fn publish_runtime(&mut self) {
        let pending = match self.run.active() {
            Some(run) => run.steering.pending_count().await,
            None => 0,
        };
        self.emit_runtime_changed(pending);
    }

    /// Override the reasoning effort restored for this session.
    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }

    /// Restore the complete persisted control state before the actor is spawned.
    pub fn with_control_state(mut self, state: SessionControlState) -> Self {
        self.mode = state.active_mode;
        self.reasoning_effort = state.reasoning_effort;
        self.control_state = Some(state);
        self
    }

    /// Attach a [`MeshHandle`] to this actor (builder pattern, for remote sessions).
    ///
    /// Called by `RemoteNodeManager` after `new()` so the `SubscribeEvents`
    /// handler can reach the Kademlia DHT without querying global state.
    #[cfg(feature = "remote")]
    pub fn with_mesh(mut self, mesh: Option<crate::agent::remote::MeshHandle>) -> Self {
        self.mesh = mesh;
        self
    }

    fn remove_relay_forwarder(
        &mut self,
        sequence_id: u64,
        relay_dht_name: &str,
    ) -> Option<crate::agent::remote::event_forwarder::EventForwarderHandle> {
        let relay_id = self
            .relay_forwarder_handles
            .iter()
            .find(|(id, (name, _))| {
                id.sequence_id() == sequence_id && name.as_deref() == Some(relay_dht_name)
            })
            .map(|(id, _)| *id)?;
        self.relay_forwarder_handles
            .remove(&relay_id)
            .map(|(_, handle)| handle)
    }

    /// Return the original receipt for a repeated `client_input_id`, if it is
    /// still within the bounded cache window.
    fn cached_submit_receipt(
        &mut self,
        client_input_id: &str,
        authority: &TurnAuthority,
    ) -> Option<SubmitInputResult> {
        self.prune_submit_receipts(now_ms());
        self.submit_receipts
            .iter()
            .find(|receipt| {
                receipt.client_input_id == client_input_id
                    && receipt.authority.same_origin(authority)
            })
            .map(|receipt| receipt.result.clone())
    }

    /// Store an accepted submission receipt before it is returned to the
    /// caller so a retried submission with the same ID is deduplicated
    /// instead of enqueueing a duplicate turn.
    fn record_submit_receipt(
        &mut self,
        client_input_id: Option<String>,
        result: &SubmitInputResult,
        authority: TurnAuthority,
    ) {
        let Some(client_input_id) = client_input_id else {
            return;
        };
        let now = now_ms();
        self.prune_submit_receipts(now);
        self.submit_receipts.push_back(SubmitInputReceipt {
            client_input_id,
            authority,
            result: result.clone(),
            accepted_at_ms: now,
        });
        while self.submit_receipts.len() > MAX_SUBMIT_RECEIPTS {
            self.submit_receipts.pop_front();
        }
    }

    fn prune_submit_receipts(&mut self, now_ms_value: u64) {
        self.submit_receipts.retain(|receipt| {
            now_ms_value.saturating_sub(receipt.accepted_at_ms) <= MAX_SUBMIT_RECEIPT_AGE_MS
        });
    }

    /// Take this session's cross-process run lease before a run starts and
    /// keep it in `self.run_lease`.
    ///
    /// `Ok(())` without a lease means leasing is unavailable (no lease root,
    /// or an I/O failure, logged): the run proceeds unleased. A lease still
    /// held by another owner after [`RUN_LEASE_GRACE`] rejects the run.
    async fn acquire_run_lease(&mut self) -> Result<(), AgentError> {
        // Already held: a queued prompt inherits the lease of the run that
        // just finished, so no other owner can slip in between turns.
        if self.run_lease.is_some() {
            return Ok(());
        }
        let Some(root) = self.config.storage.run_lease_root() else {
            return Ok(());
        };
        match RunLeaseRoot::new(root)
            .acquire_with_grace(&self.session_id, RUN_LEASE_GRACE)
            .await
        {
            Ok(Some(lease)) => {
                // A delete may have completed while this acquisition waited
                // out the grace period. Re-check under the lease so a deleted
                // session never starts a run (and never recreates its lease
                // file). Lookup failures are not treated as deletion.
                if matches!(
                    self.config
                        .provider
                        .history_store()
                        .get_session(&self.session_id)
                        .await,
                    Ok(None)
                ) {
                    drop(lease);
                    return Err(AgentError::SessionNotFound {
                        session_id: self.session_id.clone(),
                    });
                }
                self.run_lease = Some(lease);
                Ok(())
            }
            Ok(None) => Err(AgentError::SessionRunningElsewhere {
                session_id: self.session_id.clone(),
            }),
            // Session ids that are not file-name safe cannot be leased; that
            // is a property of the id, not a fault worth warning about.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
                debug!(
                    "Session {}: run lease not applicable, running unleased: {}",
                    self.session_id, error
                );
                Ok(())
            }
            Err(error) => {
                warn!(
                    "Session {}: run lease unavailable, running unleased: {}",
                    self.session_id, error
                );
                Ok(())
            }
        }
    }

    /// Start a run of `req`. The only place that fills the run slot: it
    /// emits `RunStarted` with the origin's label (then `QueuedInputStarted`
    /// for a queued input), spawns the phase forwarder and the execution task.
    /// What the run emits when it ends depends on `origin`; `completion`
    /// carries the reply. `turn_id` is the id the starter chose for the run
    /// (a scheduled run has one its scheduler knows); a fresh one otherwise.
    fn start_run(
        &mut self,
        origin: ExecutionOrigin,
        req: crate::acp::protocol::PromptRequest,
        completion: Completion,
        turn_id: Option<querymt_agent_core::model::TurnId>,
        actor_ref: kameo::actor::ActorRef<SessionActor>,
        authority: TurnAuthority,
    ) {
        self.generation = self.generation.saturating_add(1);
        let generation = self.generation;
        let turn_id = turn_id.unwrap_or_else(querymt_agent_core::model::TurnId::new);
        let run_id = turn_id;
        let active_run = ActiveRun::new(run_id, generation, CancellationToken::new());
        let steering = active_run.steering.clone();
        let cancel_token = active_run.cancellation.clone();
        self.run = RunSlot::Running(active_run, authority.clone());
        self.emit_runtime_changed(0);

        // After the lease is held and before any other event of the run.
        self.config.emit_event(
            &self.session_id,
            AgentEventKind::RunStarted {
                run_id: run_id.to_string(),
                origin: origin.label().to_string(),
            },
        );
        if let Some(input_id) = &completion.queued_input_id {
            self.config.emit_event(
                &self.session_id,
                AgentEventKind::QueuedInputStarted {
                    input_id: input_id.clone(),
                    run_id: run_id.to_string(),
                },
            );
        }

        let (phase_tx, phase_rx) = tokio::sync::watch::channel(RunSignal::starting());
        tokio::spawn(forward_run_signals(
            phase_rx,
            actor_ref.clone(),
            generation,
            run_id,
        ));

        let exec = DetachedPromptExecution {
            req,
            session_id: self.session_id.clone(),
            runtime: self.runtime.clone(),
            config: self.config.clone(),
            cancel_token: cancel_token.clone(),
            mode: self.mode,
            tool_config: self.tool_config.clone(),
            execution_origin: origin.clone(),
            turn_id,
            steering,
            phase_reporter: phase_tx,
        };
        let config = self.config.clone();
        let session_id = self.session_id.clone();
        let event_stream = config.event_sink.session_stream(&session_id);
        // A scheduled run's task stays a child of the span of the message that
        // started it.
        let span = match &origin {
            ExecutionOrigin::Interactive => tracing::Span::none(),
            ExecutionOrigin::Scheduled { .. } => info_span!(
                parent: &tracing::Span::current(),
                "agent.scheduled_prompt.task",
                session_id = %session_id,
                prompt_generation = generation,
                execution_origin = "scheduled",
                mode = %self.mode,
            ),
        };
        tokio::spawn(
            async move {
                let run =
                    execute_prompt_guarded(exec, &authority, &config, &session_id, run_id).await;
                let disposition = run_disposition(cancel_token.is_cancelled(), &run);
                let finished = PromptFinished {
                    generation,
                    run_id,
                    disposition,
                };
                let Completion { reply, .. } = completion;
                match origin {
                    ExecutionOrigin::Interactive => {
                        // The reply carries the last `session_seq` the run
                        // produced, so a transport can hold the response until
                        // its connection has delivered every update of this
                        // prompt.
                        let tip = event_stream.resolved_tip().await;
                        let result =
                            run.map(|run| crate::acp::delivery::with_event_tip(run.response, tip));
                        let _ = reply.send(result);
                        let _ = actor_ref.tell(finished).await;
                    }
                    ExecutionOrigin::Scheduled {
                        schedule_public_id, ..
                    } => {
                        // The scheduler correlates cycles by these terminal
                        // events, which carry the run's own id.
                        match &run {
                            Ok(_) => config.emit_event(
                                &session_id,
                                AgentEventKind::ScheduledExecutionCompleted {
                                    schedule_public_id,
                                    turn_id: run_id.to_string(),
                                },
                            ),
                            Err(error) => config.emit_event(
                                &session_id,
                                AgentEventKind::ScheduledExecutionFailed {
                                    schedule_public_id,
                                    turn_id: Some(run_id.to_string()),
                                    error: error.to_string(),
                                },
                            ),
                        }
                        if let Err(error) = actor_ref.tell(finished).await {
                            warn!(
                                "Failed to send PromptFinished after scheduled execution: {:?}",
                                error
                            );
                        }
                        let _ = reply.send(run.map(|run| run.response));
                    }
                }
            }
            .instrument(span),
        );
    }

    /// Helper method to extract system prompt from current session config.
    async fn get_session_system_prompt(&self) -> Vec<querymt::params::SystemPrompt> {
        if let Ok(Some(current_config)) = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&self.session_id)
            .await
            && let Some(params) = &current_config.params
            && let Some(system_array) = params.get("system").and_then(|v| v.as_array())
        {
            let mut system_parts = Vec::new();
            for item in system_array {
                if let Ok(part) =
                    serde_json::from_value::<querymt::params::SystemPrompt>(item.clone())
                {
                    system_parts.push(part);
                }
            }
            if !system_parts.is_empty() {
                return system_parts;
            }
        }
        self.config.provider.initial_config().system.clone()
    }
}

// ══════════════════════════════════════════════════════════════════════════
//  Message Handlers
// ══════════════════════════════════════════════════════════════════════════

// ── Cancel ───────────────────────────────────────────────────────────────
//
// Cancellation flow:
// 1. Cancel message cancels the running run's token.
// 2. Detached prompt task observes token.is_cancelled() and exits early.
// 3. PromptFinished applies only when generation matches the current turn.
// 4. Matching PromptFinished empties the run slot.

impl Message<Cancel> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(&mut self, msg: Cancel, _: &mut Context<Self, Self::Reply>) -> Self::Reply {
        msg.authority.check().await?;
        if let Some(authority) = self.run.authority() {
            if !msg.authority.can_control(authority) {
                return Err(AgentError::MeshAccessDenied);
            }
        } else if !matches!(msg.authority, TurnAuthority::Local) {
            return Ok(());
        }

        debug!("Session {}: Cancel received", self.session_id);
        self.run.cancel();
        let cancelled_elicitations = crate::elicitation::cancel_pending_elicitations_for_session(
            &self.config.pending_elicitations,
            &self.session_id,
        )
        .await;
        if cancelled_elicitations > 0 {
            debug!(
                "Session {}: cancelled {} pending elicitation(s)",
                self.session_id, cancelled_elicitations
            );
        }
        if let Some(run) = self.run.active_mut() {
            run.phase = RunPhase::Closing;
            let run_id = run.run_id.to_string();
            for input in run.steering.discard().await {
                self.config.emit_event(
                    &self.session_id,
                    AgentEventKind::SteeringDiscarded {
                        run_id: run_id.clone(),
                        input_id: input.input_id,
                        reason: "run_cancelled".to_string(),
                    },
                );
            }
        } else {
            // No run in this process: an unmatched run_started can only be an
            // orphan of a dead process. Reconcile off the mailbox.
            let config = self.config.clone();
            let session_id = self.session_id.clone();
            tokio::spawn(async move {
                crate::session::run_reconcile::reconcile_orphaned_run(&config, &session_id).await;
            });
        }
        self.config
            .emit_event(&self.session_id, AgentEventKind::Cancelled);
        self.publish_runtime().await;
        Ok(())
    }
}

// ── PromptFinished (internal) ────────────────────────────────────────────

impl Message<RunPhaseChanged> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: RunPhaseChanged,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(run) = self.run.active_mut()
            && run.generation == msg.generation
            && run.run_id == msg.run_id
        {
            if let Some(phase) = msg.phase {
                run.phase = phase;
            }
            self.publish_runtime().await;
        }
    }
}

impl Message<PromptFinished> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: PromptFinished,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if self
            .run
            .active()
            .is_none_or(|run| run.generation != msg.generation || run.run_id != msg.run_id)
        {
            debug!(
                "Session {} PromptFinished: stale run ignored",
                self.session_id
            );
            return;
        }

        // Steering accepted but not applied before the run ended (it failed,
        // or panicked, or stopped before its next boundary) would otherwise
        // have no terminal event. A run that ended normally closed its inbox
        // already, so this finds nothing for it.
        if let Some(run) = self.run.active() {
            let steering = run.steering.clone();
            let reason = match msg.disposition {
                RunCompletionDisposition::Failed => "run_failed",
                _ => "run_ended",
            };
            for input in steering.discard().await {
                self.config.emit_event(
                    &self.session_id,
                    AgentEventKind::SteeringDiscarded {
                        run_id: msg.run_id.to_string(),
                        input_id: input.input_id,
                        reason: reason.to_string(),
                    },
                );
            }
        }

        let (outcome, stop_type, reason) = match msg.disposition {
            RunCompletionDisposition::Completed => (RunOutcome::Completed, None, None),
            RunCompletionDisposition::Failed => (RunOutcome::Failed, None, None),
            RunCompletionDisposition::Cancelled => (RunOutcome::Cancelled, None, None),
            RunCompletionDisposition::Stopped { stop_type, reason } => {
                (RunOutcome::Stopped, Some(stop_type), Some(reason))
            }
        };
        // The run lease is released only after the completion is recorded, so
        // a reconciler never sees a free lease with an unmatched run_started
        // that this process is still about to close.
        if let Err(error) = self
            .config
            .emit_event_persisted(
                &self.session_id,
                AgentEventKind::RunCompleted {
                    run_id: msg.run_id.to_string(),
                    outcome: outcome.into(),
                    stop_type,
                    reason,
                },
            )
            .await
        {
            warn!(
                "Session {}: failed to persist run completion: {}",
                self.session_id, error
            );
        }

        // The turn persisted messages, so the session's last-activity time
        // changed. Read the persisted row so the patch matches later
        // session-list output exactly, and emit only after that read succeeds.
        if let Ok(Some(session)) = self
            .config
            .provider
            .history_store()
            .get_session(&self.session_id)
            .await
        {
            let updated_at = session
                .updated_at
                .and_then(|updated_at| {
                    updated_at
                        .format(&time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .unwrap_or_default();
            self.config.emit_event(
                &self.session_id,
                AgentEventKind::SessionMetadataUpdated {
                    title: None,
                    updated_at: Some(updated_at),
                },
            );
        }

        self.run = RunSlot::Idle;

        // Hand the lease over to the next queued prompt instead of releasing
        // it, so no other owner can start a run between two queued turns.
        // The completion above is already persisted, so a reconciler never
        // sees a free lease with this run still open.
        while let Some(queued) = self.queued_prompts.pop_front() {
            if let Err(error) = queued.authority.check().await {
                self.config.emit_event(
                    &self.session_id,
                    AgentEventKind::QueuedInputDiscarded {
                        input_id: queued.input_id,
                        reason: "mesh_access_denied".into(),
                    },
                );
                let _ = queued.reply.send(Err(error));
                continue;
            }
            self.start_run(
                ExecutionOrigin::Interactive,
                queued.req,
                Completion {
                    reply: queued.reply,
                    queued_input_id: Some(queued.input_id),
                },
                None,
                ctx.actor_ref().clone(),
                queued.authority,
            );
            return;
        }
        self.run_lease = None;
        self.publish_runtime().await;
    }
}

// ── SetMode ──────────────────────────────────────────────────────────────

impl Message<SetMode> for SessionActor {
    type Reply = SetModeReply;

    async fn handle(&mut self, msg: SetMode, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        self.transition_mode(msg.mode).await
    }
}

// ── GetMode ──────────────────────────────────────────────────────────────

impl Message<GetMode> for SessionActor {
    type Reply = Result<AgentMode, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: GetMode,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.mode)
    }
}

// ── SetReasoningEffort ────────────────────────────────────────────────────

impl Message<SetReasoningEffort> for SessionActor {
    type Reply = SetReasoningEffortReply;

    async fn handle(
        &mut self,
        msg: SetReasoningEffort,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.transition_reasoning_effort(msg.effort).await
    }
}

// ── GetReasoningEffort ───────────────────────────────────────────────────

impl Message<GetReasoningEffort> for SessionActor {
    type Reply = Result<Option<ReasoningEffort>, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: GetReasoningEffort,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.reasoning_effort)
    }
}

// ── SetProvider ──────────────────────────────────────────────────────────

impl Message<SetProvider> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: SetProvider,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let selection = crate::agent::session_control::SessionModelSelection {
            model_id: format!("{}/{}", msg.provider, msg.model),
            provider_node_id: None,
        };
        self.transition_model(selection).await.map(|_| ())
    }
}

// ── SetLlmConfig ─────────────────────────────────────────────────────────

impl Message<SetLlmConfig> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: SetLlmConfig,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let model_id = format!(
            "{}/{}",
            msg.config
                .provider
                .as_deref()
                .ok_or(AgentError::ProviderRequired)?,
            msg.config.model.as_deref().unwrap_or_default()
        );
        self.transition_model_with_config(model_id, msg.provider_node_id, msg.config)
            .await
            .map(|_| ())
    }
}

impl SessionActor {
    fn apply_control_state(&mut self, state: SessionControlState) -> SessionControlState {
        self.mode = state.active_mode;
        self.reasoning_effort = state.reasoning_effort;
        self.control_state = Some(state.clone());
        state
    }

    async fn load_control_state_from_store(&mut self) -> Result<SessionControlState, AgentError> {
        let store = self.config.provider.history_store();
        if let Some(state) = store.get_session_control(&self.session_id).await? {
            return Ok(self.apply_control_state(state));
        }

        let config = match store.get_session_llm_config(&self.session_id).await? {
            Some(config) => config,
            None => {
                store
                    .create_or_get_llm_config(self.config.provider.initial_config())
                    .await?
            }
        };
        let provider_node_id = store.get_session_provider_node_id(&self.session_id).await?;
        let binding = SessionModelBinding {
            model_id: format!("{}/{}", config.provider, config.model),
            provider: config.provider,
            model: config.model,
            llm_config_id: config.id,
            provider_node_id,
        };
        let mut mode_models = HashMap::new();
        for mode in [AgentMode::Build, AgentMode::Plan, AgentMode::Review] {
            mode_models.insert(mode.as_str().to_string(), binding.clone());
        }
        let state = SessionControlState {
            revision: 1,
            active_mode: self.mode,
            reasoning_effort: self.reasoning_effort,
            effective_model: binding,
            mode_models,
        };
        match store
            .commit_session_control(&self.session_id, 0, &state)
            .await
        {
            Ok(state) => Ok(self.apply_control_state(state)),
            Err(error) => {
                let agent_error = AgentError::from(error);
                if agent_error.is_session_control_revision_conflict()
                    && let Some(existing) = store.get_session_control(&self.session_id).await?
                {
                    return Ok(self.apply_control_state(existing));
                }
                Err(agent_error)
            }
        }
    }

    async fn ensure_control_state(&mut self) -> Result<SessionControlState, AgentError> {
        if let Some(state) = &self.control_state {
            return Ok(state.clone());
        }
        self.load_control_state_from_store().await
    }

    async fn reload_control_state(&mut self) -> Result<SessionControlState, AgentError> {
        self.control_state = None;
        self.load_control_state_from_store().await
    }

    async fn commit_control_transition(
        &mut self,
        previous: SessionControlState,
        mut proposed: SessionControlState,
    ) -> Result<SessionControlTransition, AgentError> {
        proposed.revision = previous.revision.saturating_add(1);
        let current = match self
            .config
            .provider
            .history_store()
            .commit_session_control(&self.session_id, previous.revision, &proposed)
            .await
        {
            Ok(current) => current,
            Err(error) => return Err(AgentError::from(error)),
        };

        self.apply_control_state(current.clone());

        let mode_changed = previous.active_mode != current.active_mode;
        let model_changed = previous.effective_model != current.effective_model;
        let effort_changed = previous.reasoning_effort != current.reasoning_effort;

        if mode_changed {
            self.config.emit_event(
                &self.session_id,
                AgentEventKind::SessionModeChanged {
                    mode: current.active_mode,
                },
            );
        }
        if model_changed {
            let binding = &current.effective_model;
            let context_limit = querymt_agent_services::model_info::get_model_info(
                &binding.provider,
                &binding.model,
            )
            .and_then(|model| model.context_limit());
            self.config.emit_event(
                &self.session_id,
                AgentEventKind::ProviderChanged {
                    provider: binding.provider.clone(),
                    model: binding.model.clone(),
                    config_id: binding.llm_config_id,
                    context_limit,
                    provider_node_id: binding.provider_node_id.clone(),
                },
            );
        }
        // One authoritative post-commit configuration snapshot whenever any
        // effective value changed; no-op transitions emit nothing. Mode changes
        // additionally emit SessionModeChanged above so projections can order
        // `current_mode_update` before the full config replacement.
        if mode_changed || model_changed || effort_changed {
            self.config.emit_event(
                &self.session_id,
                AgentEventKind::SessionConfigChanged {
                    mode: current.active_mode,
                    reasoning_effort: current.reasoning_effort.map(|effort| effort.to_string()),
                },
            );
        }

        Ok(SessionControlTransition { previous, current })
    }

    async fn rebase_control_once(
        &mut self,
        intent: ControlTransitionIntent,
    ) -> Result<SessionControlTransition, AgentError> {
        let previous = self.ensure_control_state().await?;
        match self.apply_control_intent(previous, &intent, false).await {
            Ok(transition) => Ok(transition),
            Err(error) if error.is_session_control_revision_conflict() => {
                let previous = self.reload_control_state().await?;
                self.apply_control_intent(previous, &intent, true).await
            }
            Err(error) => Err(error),
        }
    }

    async fn apply_control_intent(
        &mut self,
        previous: SessionControlState,
        intent: &ControlTransitionIntent,
        rebased: bool,
    ) -> Result<SessionControlTransition, AgentError> {
        match intent {
            ControlTransitionIntent::Mode(mode) => {
                self.commit_mode_transition(previous, *mode).await
            }
            ControlTransitionIntent::Model(selection) => {
                self.apply_model_selection(previous, selection).await
            }
            ControlTransitionIntent::ModelConfig {
                model_id,
                provider_node_id,
                params,
            } => {
                let mut params = params.as_ref().clone();
                // After a conflict, only fill in the effort the caller left unset;
                // an explicitly supplied effort is preserved.
                if rebased && params.reasoning_effort.is_none() {
                    params.reasoning_effort = previous.reasoning_effort;
                }
                self.apply_model_binding(
                    previous,
                    model_id.clone(),
                    provider_node_id.clone(),
                    params,
                )
                .await
            }
            ControlTransitionIntent::ReasoningEffort(effort) => {
                self.commit_reasoning_effort(previous, *effort).await
            }
        }
    }

    async fn transition_mode(
        &mut self,
        mode: AgentMode,
    ) -> Result<SessionControlTransition, AgentError> {
        self.rebase_control_once(ControlTransitionIntent::Mode(mode))
            .await
    }

    async fn commit_mode_transition(
        &mut self,
        previous: SessionControlState,
        mode: AgentMode,
    ) -> Result<SessionControlTransition, AgentError> {
        let mut proposed = previous.clone();
        let binding = proposed
            .binding_for(mode)
            .cloned()
            .unwrap_or_else(|| previous.effective_model.clone());
        proposed.set_binding(mode, binding.clone());
        proposed.active_mode = mode;
        proposed.effective_model = binding;
        self.commit_control_transition(previous, proposed).await
    }

    async fn transition_model(
        &mut self,
        selection: crate::agent::session_control::SessionModelSelection,
    ) -> Result<SessionControlTransition, AgentError> {
        self.rebase_control_once(ControlTransitionIntent::Model(selection))
            .await
    }

    async fn transition_model_with_config(
        &mut self,
        model_id: String,
        provider_node_id: Option<String>,
        params: querymt::LLMParams,
    ) -> Result<SessionControlTransition, AgentError> {
        self.rebase_control_once(ControlTransitionIntent::ModelConfig {
            model_id,
            provider_node_id,
            params: Box::new(params),
        })
        .await
    }

    async fn apply_model_selection(
        &mut self,
        previous: SessionControlState,
        selection: &crate::agent::session_control::SessionModelSelection,
    ) -> Result<SessionControlTransition, AgentError> {
        let (provider, model) =
            parse_transport_model_id(&selection.model_id, &previous.effective_model.provider);
        if self.is_current_binding(
            &previous.effective_model,
            &provider,
            &model,
            selection.provider_node_id.as_deref(),
        ) {
            return Ok(SessionControlTransition {
                current: previous.clone(),
                previous,
            });
        }
        let mut params = self.config.provider.selection_params(&provider, &model);
        params.system = self.get_session_system_prompt().await;
        params.reasoning_effort = previous.reasoning_effort;
        self.apply_model_binding(
            previous,
            selection.model_id.clone(),
            selection.provider_node_id.clone(),
            params,
        )
        .await
    }

    /// Whether `node` designates this node: no node, `local`, or our own peer id.
    fn is_local_node(&self, node: Option<&str>) -> bool {
        let Some(node) = node else {
            return true;
        };
        if node == "local" {
            return true;
        }
        #[cfg(feature = "remote")]
        if let Some(mesh) = &self.mesh {
            return mesh.peer_id().to_string() == node;
        }
        false
    }

    /// Whether a selection names the provider, model and node `current` is bound to.
    fn is_current_binding(
        &self,
        current: &SessionModelBinding,
        provider: &str,
        model: &str,
        node: Option<&str>,
    ) -> bool {
        let current_node = current.provider_node_id.as_deref();
        current.provider == provider
            && current.model == model
            && (current_node == node
                || (self.is_local_node(current_node) && self.is_local_node(node)))
    }

    async fn apply_model_binding(
        &mut self,
        previous: SessionControlState,
        model_id: String,
        provider_node_id: Option<String>,
        params: querymt::LLMParams,
    ) -> Result<SessionControlTransition, AgentError> {
        let provider_name = params
            .provider
            .as_ref()
            .ok_or(AgentError::ProviderRequired)?;
        let plugin_registry = self.config.provider.plugin_registry();
        let is_known_local_provider = plugin_registry
            .list_provider_names()
            .contains(&provider_name.as_str())
            || plugin_registry.get(provider_name).await.is_some();
        if !is_known_local_provider && provider_node_id.is_none() {
            return Err(AgentError::UnknownProvider {
                name: provider_name.clone(),
            });
        }
        let config = self
            .config
            .provider
            .history_store()
            .create_or_get_llm_config(&params)
            .await?;
        let binding = SessionModelBinding {
            model_id,
            provider: config.provider,
            model: config.model,
            llm_config_id: config.id,
            provider_node_id,
        };
        let mut proposed = previous.clone();
        proposed.set_binding(previous.active_mode, binding.clone());
        proposed.effective_model = binding;
        self.commit_control_transition(previous, proposed).await
    }

    async fn transition_reasoning_effort(
        &mut self,
        effort: Option<ReasoningEffort>,
    ) -> Result<SessionControlTransition, AgentError> {
        self.rebase_control_once(ControlTransitionIntent::ReasoningEffort(effort))
            .await
    }

    async fn commit_reasoning_effort(
        &mut self,
        previous: SessionControlState,
        effort: Option<ReasoningEffort>,
    ) -> Result<SessionControlTransition, AgentError> {
        let store = self.config.provider.history_store();
        let mut proposed = previous.clone();
        proposed.reasoning_effort = effort;
        let mut mode_models = previous.mode_models.clone();
        mode_models
            .entry(previous.active_mode.as_str().to_string())
            .or_insert_with(|| previous.effective_model.clone());
        let mut updated = HashMap::new();
        for (mode, binding) in &mode_models {
            let current = store
                .get_llm_config(binding.llm_config_id)
                .await?
                .ok_or_else(|| {
                    AgentError::Internal(format!(
                        "missing LLM config {} for mode {mode}",
                        binding.llm_config_id
                    ))
                })?;
            let mut params = current
                .params
                .as_ref()
                .and_then(|value| serde_json::from_value::<querymt::LLMParams>(value.clone()).ok())
                .unwrap_or_default()
                .provider(&current.provider)
                .model(&current.model);
            params.reasoning_effort = effort;
            let config = store.create_or_get_llm_config(&params).await?;
            let mut binding = binding.clone();
            binding.llm_config_id = config.id;
            updated.insert(mode.clone(), binding);
        }
        proposed.mode_models = updated;
        proposed.effective_model = proposed
            .binding_for(previous.active_mode)
            .cloned()
            .ok_or_else(|| AgentError::Internal("active mode has no model binding".to_string()))?;
        self.commit_control_transition(previous, proposed).await
    }
}

impl Message<GetSessionControl> for SessionActor {
    type Reply = GetSessionControlReply;

    async fn handle(
        &mut self,
        _msg: GetSessionControl,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.ensure_control_state().await
    }
}

// ── SetSessionModel ──────────────────────────────────────────────────────

impl Message<SetSessionModel> for SessionActor {
    type Reply = SetSessionModelReply;

    async fn handle(
        &mut self,
        msg: SetSessionModel,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.transition_model(msg.selection).await
    }
}

// ── Tool Config Messages ─────────────────────────────────────────────────

impl Message<SetToolPolicy> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetToolPolicy,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.policy = msg.policy;
    }
}

impl Message<SetAllowedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetAllowedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.allow = Some(msg.tools.into_iter().collect());
    }
}

impl Message<ClearAllowedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: ClearAllowedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.allow = None;
    }
}

impl Message<SetDeniedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: SetDeniedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.deny = msg.tools.into_iter().collect();
    }
}

impl Message<ClearDeniedTools> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: ClearDeniedTools,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.tool_config.deny = Default::default();
    }
}

// ── State Queries ────────────────────────────────────────────────────────

impl Message<GetSessionLimits> for SessionActor {
    type Reply = Result<Option<SessionLimits>, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: GetSessionLimits,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(self.config.get_session_limits())
    }
}

impl Message<crate::agent::messages::GetSessionSlashRegistry> for SessionActor {
    type Reply = Result<Arc<crate::slash_commands::SlashCommandRegistry>, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: crate::agent::messages::GetSessionSlashRegistry,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(Arc::new(
            self.runtime
                .effective_slash_registry(&self.config.slash_command_registry)
                .clone(),
        ))
    }
}

impl Message<GetRuntimeStatus> for SessionActor {
    type Reply = Result<SessionRuntimeStatus, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        msg: GetRuntimeStatus,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let pending_steering_count = match self.run.active() {
            Some(run) => run.steering.pending_count().await,
            None => 0,
        };
        if pending_steering_count != self.published_pending_steering {
            self.emit_runtime_changed(pending_steering_count);
        }
        let mut status = self.runtime_status(pending_steering_count);
        status.steerable &= self
            .run
            .authority()
            .is_some_and(|authority| msg.authority.can_control(authority));
        Ok(status)
    }
}

impl Message<SessionDeliveryBarrier> for SessionActor {
    type Reply = Result<SessionDeliveryObservation, kameo::error::Infallible>;

    async fn handle(
        &mut self,
        _msg: SessionDeliveryBarrier,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let runtime = self.runtime_status(self.published_pending_steering);
        let tip = self
            .config
            .event_sink
            .session_stream(&self.session_id)
            .emit_ephemeral_seq(AgentEventKind::RuntimeChanged {
                runtime: runtime.clone(),
            })
            .await;
        Ok(SessionDeliveryObservation { runtime, tip })
    }
}

impl Message<GetLlmConfig> for SessionActor {
    type Reply = Result<Option<LLMConfig>, AgentError>;

    async fn handle(
        &mut self,
        _msg: GetLlmConfig,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.config
            .provider
            .history_store()
            .get_session_llm_config(&self.session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))
    }
}

// ── Undo / Redo ──────────────────────────────────────────────────────────

impl Message<Undo> for SessionActor {
    type Reply = Result<UndoResult, UndoError>;

    async fn handle(&mut self, msg: Undo, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let backend = self
            .config
            .snapshot_backend
            .as_ref()
            .ok_or(UndoError::NoSnapshotBackend)?;

        let worktree = self
            .runtime
            .cwd
            .as_ref()
            .ok_or(UndoError::NoWorkingDirectory)?
            .to_path_buf();

        // Delegate to free function using our owned state, under the
        // checkout mutation gate when one is configured so undo never
        // interleaves with baseline capture or cooperating tool writes.
        let fut = crate::agent::undo::undo_impl(
            backend.as_ref(),
            &self.config.provider,
            &self.session_id,
            &msg.message_id,
            &worktree,
        );
        match self.runtime.mutation_gate.get() {
            Some(gate) => match gate.run(fut).await {
                Ok(inner) => inner,
                Err(err) => Err(UndoError::Other(err.to_string())),
            },
            None => fut.await,
        }
    }
}

impl Message<Redo> for SessionActor {
    type Reply = Result<RedoResult, UndoError>;

    async fn handle(&mut self, _msg: Redo, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let backend = self
            .config
            .snapshot_backend
            .as_ref()
            .ok_or(UndoError::NoSnapshotBackend)?;

        let worktree = self
            .runtime
            .cwd
            .as_ref()
            .ok_or(UndoError::NoWorkingDirectory)?
            .to_path_buf();

        // Restore under the checkout mutation gate when one is configured.
        let fut = crate::agent::undo::redo_impl(
            backend.as_ref(),
            &self.config.provider,
            &self.session_id,
            &worktree,
        );
        match self.runtime.mutation_gate.get() {
            Some(gate) => match gate.run(fut).await {
                Ok(inner) => inner,
                Err(err) => Err(UndoError::Other(err.to_string())),
            },
            None => fut.await,
        }
    }
}

impl Message<ForkAtMessage> for SessionActor {
    type Reply = Result<String, AgentError>;

    async fn handle(
        &mut self,
        msg: ForkAtMessage,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.config
            .provider
            .history_store()
            .fork_session(
                &self.session_id,
                &msg.message_id,
                crate::session::domain::ForkOrigin::User,
            )
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))
    }
}

impl Message<ExtMethod> for SessionActor {
    type Reply = Result<ExtResponse, AgentError>;

    async fn handle(
        &mut self,
        _msg: ExtMethod,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let raw_value = serde_json::value::RawValue::from_string("null".to_string())
            .map_err(|e| AgentError::Serialization(e.to_string()))?;
        Ok(ExtResponse::new(std::sync::Arc::from(raw_value)))
    }
}

impl Message<ExtNotification> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        _msg: ExtNotification,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        Ok(())
    }
}

impl Message<ListPendingInteractions> for SessionActor {
    type Reply = Result<Vec<crate::elicitation::PendingElicitationSnapshot>, AgentError>;

    async fn handle(
        &mut self,
        msg: ListPendingInteractions,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let map = &self.config.pending_elicitations;
        if let Some(authority) = msg.assign_authority.as_deref() {
            crate::elicitation::assign_authority_in_map(map, &self.session_id, authority).await;
        }
        Ok(crate::elicitation::snapshot_pending_elicitations(map)
            .await
            .into_iter()
            .filter(|snapshot| snapshot.session_id == self.session_id)
            .collect())
    }
}

impl Message<ClaimInteractionDelivery> for SessionActor {
    type Reply = Result<Vec<crate::elicitation::ClaimedElicitationDelivery>, AgentError>;

    async fn handle(
        &mut self,
        msg: ClaimInteractionDelivery,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if msg.authority.is_none() && msg.interaction_id.is_none() {
            return Err(AgentError::InvalidInteractionResponse {
                detail: "claim requires an interaction id or an authority".to_string(),
            });
        }
        Ok(crate::elicitation::claim_deliveries_in_map(
            &self.config.pending_elicitations,
            &self.session_id,
            msg.interaction_id.as_deref(),
            msg.authority.as_deref(),
            &msg.connection_id,
        )
        .await)
    }
}

impl Message<ResolveInteraction> for SessionActor {
    type Reply = Result<crate::elicitation::ElicitationResolution, AgentError>;

    async fn handle(
        &mut self,
        msg: ResolveInteraction,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        crate::elicitation::resolve_in_map(
            &self.config.pending_elicitations,
            Some(&self.session_id),
            &msg.interaction_id,
            &msg.connection_id,
            msg.delivery_generation,
            msg.response,
        )
        .await
        .map_err(interaction_response_error)
    }
}

fn interaction_response_error(error: crate::acp::protocol::Error) -> AgentError {
    AgentError::InvalidInteractionResponse {
        detail: match error.data {
            Some(serde_json::Value::String(data)) => data,
            _ => error.message,
        },
    }
}

// Only the authenticated ingress adapter constructs this mailbox message.
#[cfg(feature = "remote")]
pub(crate) struct PeerInteraction<M> {
    pub(crate) peer: Arc<PeerTurn>,
    pub(crate) message: M,
}

#[cfg(feature = "remote")]
struct PeerInteractionScope<'a> {
    session_id: &'a str,
    run_id: String,
    prefix: String,
}

#[cfg(feature = "remote")]
impl PeerInteractionScope<'_> {
    fn matches(&self, entry: &crate::elicitation::PendingElicitation) -> bool {
        entry.session_id == self.session_id
            && entry.run_id.as_deref() == Some(self.run_id.as_str())
            && entry
                .owner_authority
                .as_deref()
                .is_none_or(|id| id.starts_with(&self.prefix))
            && entry
                .delivery_connection
                .as_deref()
                .is_none_or(|id| id.starts_with(&self.prefix))
    }

    fn bind(&self, id: &str) -> String {
        format!("{}{id}", self.prefix)
    }

    fn unbind(&self, id: Option<String>) -> Option<String> {
        id.and_then(|id| id.strip_prefix(&self.prefix).map(str::to_owned))
    }
}

#[cfg(feature = "remote")]
impl PeerTurn {
    fn check_interaction_changes(
        &self,
        changes: Option<
            tokio::sync::broadcast::Receiver<querymt_remote::peer_access::PolicyChange>,
        >,
    ) -> Result<(), AgentError> {
        use querymt_remote::peer_access::{PolicyChange, PolicyEffect};
        use tokio::sync::broadcast::error::TryRecvError;

        let Some(mut changes) = changes else {
            return Ok(());
        };
        loop {
            match changes.try_recv() {
                Ok(PolicyChange {
                    key,
                    effect: PolicyEffect::Grant(grant),
                }) if key == self.key => {
                    self.check_grant(&grant, None)?;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) => return Ok(()),
                Err(_) => return Err(AgentError::MeshAccessDenied),
            }
        }
    }
}

#[cfg(feature = "remote")]
impl SessionActor {
    async fn lock_peer_interactions(
        &self,
        peer: Arc<PeerTurn>,
    ) -> Result<
        Option<(
            PeerInteractionScope<'_>,
            tokio::sync::MutexGuard<'_, HashMap<String, crate::elicitation::PendingElicitation>>,
        )>,
        AgentError,
    > {
        let caller = TurnAuthority::Peer(peer.clone());
        let caller_changes = peer.store.subscribe_peer_changes(&peer.key);
        caller.check().await?;
        let RunSlot::Running(run, authority @ TurnAuthority::Peer(run_peer)) = &self.run else {
            return Ok(None);
        };
        if !caller.can_control(authority) {
            return Ok(None);
        }
        let run_changes = run_peer.store.subscribe_peer_changes(&run_peer.key);
        authority.check().await?;

        let entries = self.config.pending_elicitations.lock().await;
        // Recheck changes during authorization and lock contention without store I/O.
        peer.check_interaction_changes(caller_changes)?;
        run_peer.check_interaction_changes(run_changes)?;
        if run.cancellation.is_cancelled() {
            return Ok(None);
        }
        Ok(Some((
            PeerInteractionScope {
                session_id: &self.session_id,
                run_id: run.run_id.to_string(),
                // ACP verifiers and connection IDs are scoped to the sending peer.
                prefix: format!("mesh:{}:", peer.key.peer),
            },
            entries,
        )))
    }
}

#[cfg(feature = "remote")]
impl Message<PeerInteraction<ListPendingInteractions>> for SessionActor {
    type Reply = Result<Vec<crate::elicitation::PendingElicitationSnapshot>, AgentError>;

    async fn handle(
        &mut self,
        PeerInteraction { peer, message }: PeerInteraction<ListPendingInteractions>,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let Some((scope, mut entries)) = self.lock_peer_interactions(peer).await? else {
            return Ok(Vec::new());
        };
        if let Some(verifier) = message.assign_authority.as_deref() {
            let bound = scope.bind(verifier);
            for entry in entries.values_mut().filter(|entry| scope.matches(entry)) {
                if entry.owner_authority.is_none() {
                    entry.owner_authority = Some(bound.clone());
                }
            }
        }
        let mut snapshots =
            crate::elicitation::snapshot_matching(&entries, |entry| scope.matches(entry));
        for snapshot in &mut snapshots {
            snapshot.owner_authority = scope.unbind(snapshot.owner_authority.take());
            snapshot.delivery_connection = scope.unbind(snapshot.delivery_connection.take());
        }
        Ok(snapshots)
    }
}

#[cfg(feature = "remote")]
impl Message<PeerInteraction<ClaimInteractionDelivery>> for SessionActor {
    type Reply = Result<Vec<crate::elicitation::ClaimedElicitationDelivery>, AgentError>;

    async fn handle(
        &mut self,
        PeerInteraction { peer, message }: PeerInteraction<ClaimInteractionDelivery>,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if message.authority.is_none() && message.interaction_id.is_none() {
            return Err(AgentError::InvalidInteractionResponse {
                detail: "claim requires an interaction id or an authority".to_string(),
            });
        }
        let Some((scope, mut entries)) = self.lock_peer_interactions(peer).await? else {
            return Ok(Vec::new());
        };
        let verifier = message.authority.as_deref().map(|id| scope.bind(id));
        Ok(crate::elicitation::claim_matching(
            &mut entries,
            &scope.bind(&message.connection_id),
            |id, entry| {
                scope.matches(entry)
                    && message
                        .interaction_id
                        .as_deref()
                        .is_none_or(|expected| id == expected)
                    && verifier
                        .as_ref()
                        .is_none_or(|verifier| entry.owner_authority.as_ref() == Some(verifier))
            },
        ))
    }
}

#[cfg(feature = "remote")]
impl Message<PeerInteraction<ResolveInteraction>> for SessionActor {
    type Reply = Result<crate::elicitation::ElicitationResolution, AgentError>;

    async fn handle(
        &mut self,
        PeerInteraction { peer, message }: PeerInteraction<ResolveInteraction>,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::elicitation::ElicitationResolution;

        let Some((scope, mut entries)) = self.lock_peer_interactions(peer).await? else {
            return Ok(ElicitationResolution::NotFound);
        };
        let connection = scope.bind(&message.connection_id);
        crate::elicitation::resolve_matching(
            &mut entries,
            &message.interaction_id,
            &connection,
            message.delivery_generation,
            message.response,
            |entry| scope.matches(entry) && entry.delivery_connection.is_some(),
        )
        .map_err(interaction_response_error)
    }
}

impl Message<Shutdown> for SessionActor {
    type Reply = ();

    async fn handle(
        &mut self,
        _msg: Shutdown,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        ctx.stop();
    }
}

/// A peer's request to stop this runtime.
///
/// Stopping cancels the running turn and drops queued prompts, so the peer
/// must be allowed to cancel each of them, as for [`Cancel`]. The check and
/// the stop happen in one handler, so no turn can start in between, and the
/// grant is re-read here, at the last boundary before the side effect.
#[cfg(feature = "remote")]
pub(crate) struct PeerShutdown(pub(crate) Arc<PeerTurn>);

#[cfg(feature = "remote")]
impl Message<PeerShutdown> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        PeerShutdown(peer): PeerShutdown,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        peer.check_mutation().await?;
        let caller = TurnAuthority::Peer(peer);
        let foreign_run = self
            .run
            .authority()
            .is_some_and(|authority| !caller.can_control(authority));
        let foreign_queued = self
            .queued_prompts
            .iter()
            .any(|queued| !caller.can_control(&queued.authority));
        if foreign_run || foreign_queued {
            return Err(AgentError::MeshAccessDenied);
        }
        ctx.stop();
        Ok(())
    }
}

/// [`GetLlmConfig`] for a mesh peer.
///
/// Only a full grant sees the stored provider parameters and endpoint: they
/// can hold the owner's provider-specific credentials, headers, base URL and
/// system prompt, none of which a session grant bounds.
#[cfg(feature = "remote")]
pub(crate) struct GetPeerLlmConfig {
    pub(crate) full: bool,
}

#[cfg(feature = "remote")]
impl Message<GetPeerLlmConfig> for SessionActor {
    type Reply = Result<Option<LLMConfig>, AgentError>;

    async fn handle(
        &mut self,
        msg: GetPeerLlmConfig,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let config = <Self as Message<GetLlmConfig>>::handle(self, GetLlmConfig, ctx).await?;
        Ok(config.map(|mut config| {
            if !msg.full {
                config.params = None;
                config.endpoint.clear();
            }
            config
        }))
    }
}

// ── Remote-ready messages ────────────────────────────────────────────────

/// Get the full message history for this session.
///
/// Reads from the local `SessionStore`. When called remotely, the result
/// is serialized and sent back over the kameo mesh.
impl Message<crate::agent::messages::GetHistory> for SessionActor {
    type Reply = Result<Vec<AgentMessage>, AgentError>;

    async fn handle(
        &mut self,
        _msg: crate::agent::messages::GetHistory,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.config
            .provider
            .history_store()
            .get_history(&self.session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))
    }
}

/// Structured user prompt records of this session (see `GetUserPrompts`).
impl Message<crate::agent::messages::GetUserPrompts> for SessionActor {
    type Reply = Result<Vec<crate::session::load_snapshot::UserPromptRecord>, AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::GetUserPrompts,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.config
            .provider
            .history_store()
            .get_user_prompts(&self.session_id, msg.message_ids.as_deref())
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))
    }
}

/// Return the full durable event stream for this session from the local journal.
impl Message<crate::agent::messages::GetEventStream> for SessionActor {
    type Reply = Result<Vec<querymt_agent_core::events::AgentEvent>, AgentError>;

    async fn handle(
        &mut self,
        _msg: crate::agent::messages::GetEventStream,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.config
            .event_sink
            .journal()
            .load_session_stream(&self.session_id, None, None)
            .await
            .map(|events| {
                events
                    .into_iter()
                    .map(querymt_agent_core::events::AgentEvent::from)
                    .collect()
            })
            .map_err(|e| AgentError::Internal(e.to_string()))
    }
}

/// Serve one bounded page of the durable event stream after a source-side
/// cursor (plan §16).
///
/// Events carry the host journal sequence in `AgentEvent.seq`;
/// `latest_source_seq` is the host stream tip so the requesting peer can page
/// until current. Overlap between backfill pages and the live relay is
/// deduplicated requester-side via source identity.
impl Message<crate::agent::messages::GetEventStreamSince> for SessionActor {
    type Reply = Result<crate::agent::messages::EventStreamPage, AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::GetEventStreamSince,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let journal = self.config.event_sink.journal();
        // Sanitize remote pagination inputs (plan §16): cap the page at
        // MAX_EVENT_STREAM_PAGE_SIZE — the fixed backfill page bound — so a
        // hostile usize can neither wrap negative in the journal's signed i64
        // `LIMIT ?` cast nor materialize the whole stream in one reply, and
        // floor negative cursors at zero. Error mapping and response flow
        // unchanged.
        let limit = msg
            .limit
            .min(crate::agent::messages::MAX_EVENT_STREAM_PAGE_SIZE);
        let after_source_seq = msg.after_source_seq.map(|seq| seq.max(0));
        // Raw `stream_seq` order: the receiver's cursor must strictly increase.
        let events = journal
            .load_session_page(&self.session_id, after_source_seq, Some(limit))
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;
        let latest_source_seq = journal
            .max_stream_seq(&self.session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;
        Ok(crate::agent::messages::EventStreamPage {
            events: events
                .into_iter()
                .map(querymt_agent_core::events::AgentEvent::from)
                .collect(),
            latest_source_seq,
        })
    }
}

/// Serve one size-bounded chunk of the durable event stream, forward or
/// backward (see `agent::event_chunk`). Replaces per-200-event paging for
/// sync and history on peers that support it.
impl Message<crate::agent::messages::GetEventStreamChunk> for SessionActor {
    type Reply = Result<crate::agent::messages::EventStreamChunk, AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::GetEventStreamChunk,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        crate::agent::event_chunk::build_event_stream_chunk(
            &**self.config.event_sink.journal(),
            &self.session_id,
            &msg,
        )
        .await
    }
}

/// Subscribe a remote event relay to this session's events.
///
/// New peers hand off a direct relay capability. The DHT name remains a
/// compatibility fallback for legacy senders. Success acknowledges that the
/// forwarder has been installed, never merely that the request was received.
impl Message<crate::agent::messages::SubscribeEvents> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::SubscribeEvents,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        #[cfg(feature = "remote")]
        {
            self.install_event_subscription(msg, None).await
        }

        #[cfg(not(feature = "remote"))]
        {
            let _ = msg;
            Ok(())
        }
    }
}

#[cfg(feature = "remote")]
pub(crate) struct SubscribePeerEvents {
    pub(crate) message: SubscribeEvents,
    pub(crate) subscriber: crate::agent::remote::event_forwarder::PeerSubscription,
}

#[cfg(feature = "remote")]
impl Message<SubscribePeerEvents> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: SubscribePeerEvents,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.install_event_subscription(msg.message, Some(msg.subscriber))
            .await
    }
}

#[cfg(feature = "remote")]
pub(crate) struct UnsubscribePeerEvents {
    pub(crate) message: UnsubscribeEvents,
    pub(crate) peer: libp2p::PeerId,
}

#[cfg(feature = "remote")]
impl Message<UnsubscribePeerEvents> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: UnsubscribePeerEvents,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let id = kameo::actor::ActorId::new_with_peer_id(msg.message.relay_actor_id, msg.peer);
        if self
            .relay_forwarder_handles
            .get(&id)
            .is_some_and(|(name, _)| name.as_deref() == Some(msg.message.relay_dht_name.as_str()))
        {
            self.relay_forwarder_handles.remove(&id);
        }
        Ok(())
    }
}

#[cfg(feature = "remote")]
impl SessionActor {
    async fn install_event_subscription(
        &mut self,
        msg: SubscribeEvents,
        mut subscriber: Option<crate::agent::remote::event_forwarder::PeerSubscription>,
    ) -> Result<(), AgentError> {
        use crate::agent::remote::event_forwarder::EventForwarder;
        use crate::agent::remote::event_relay::EventRelayActor;

        // Unsubscribe identifies a relay by sequence ID and peer-scoped name.
        let relay_name = msg
            .relay_dht_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                AgentError::RemoteActor(
                    "SubscribeEvents requires a non-empty relay_dht_name for unsubscribe identity"
                        .to_string(),
                )
            })?;
        let relay_source = if msg.relay_ref.is_some() {
            "direct"
        } else {
            "dht"
        };
        let relay_ref = if let Some(relay_ref) = msg.relay_ref {
            relay_ref
        } else {
            let mesh = subscriber
                .as_ref()
                .map(|subscriber| &subscriber.egress)
                .or(self.mesh.as_ref())
                .ok_or(AgentError::MeshNotBootstrapped)?;
            mesh.lookup_actor::<EventRelayActor>(relay_name.to_string())
                .await
                .map_err(|error| AgentError::SwarmLookupFailed {
                    key: relay_name.to_string(),
                    reason: error.to_string(),
                })?
                .ok_or_else(|| {
                    AgentError::RemoteActor(format!("event relay '{relay_name}' is not available"))
                })?
        };

        let relay_id = relay_ref.id();
        if let Some(subscriber) = &mut subscriber
            && (relay_id.sequence_id() != msg.relay_actor_id
                || relay_id.peer_id() != Some(subscriber.origin.key().peer.as_peer_id())
                || !subscriber.is_current().await)
        {
            return Err(AgentError::MeshAccessDenied);
        }
        if let Some((_, prev_handle)) = self.relay_forwarder_handles.remove(&relay_id) {
            prev_handle.abort();
            log::debug!(
                "Session {}: SubscribeEvents relay_actor_id={} - aborted previous forwarder",
                self.session_id,
                msg.relay_actor_id
            );
        }

        let fanout = self.config.event_sink.fanout().clone();
        let guarded = subscriber.is_some();
        let handle = if guarded {
            EventForwarder::start_with_policy(
                fanout,
                relay_ref,
                format!("session:{}", self.session_id),
                self.session_id.clone(),
                subscriber,
            )
        } else {
            EventForwarder::start(
                fanout,
                relay_ref,
                format!("session:{}", self.session_id),
                self.session_id.clone(),
            )
        };
        self.relay_forwarder_handles
            .insert(relay_id, (msg.relay_dht_name, handle));

        // Re-emit readiness if indexing completed before the live path existed.
        if !guarded
            && let Some(workspace) = self.runtime.workspace_handle.get()
            && let Some(index) = workspace.file_index()
            && let Err(e) = self
                .config
                .emit_event_persisted(
                    &self.session_id,
                    querymt_agent_core::events::AgentEventKind::WorkspaceIndexReady {
                        workspace_root: index.root.display().to_string(),
                    },
                )
                .await
        {
            warn!(
                "Session {}: failed to emit WorkspaceIndexReady catch-up after SubscribeEvents: {}",
                self.session_id, e
            );
        }

        log::debug!(
            "Session {}: SubscribeEvents installed EventForwarder via {} (relay_actor_id={})",
            self.session_id,
            relay_source,
            msg.relay_actor_id,
        );
        Ok(())
    }
}

/// Unsubscribe a previously registered event relay.
///
/// Aborts the `EventForwarder` background task for `relay_actor_id`.
/// Without a swarm, this is a no-op (nothing was registered).
impl Message<crate::agent::messages::UnsubscribeEvents> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::UnsubscribeEvents,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Some(handle) = self.remove_relay_forwarder(msg.relay_actor_id, &msg.relay_dht_name) {
            handle.abort();
            info!(
                "Session {}: UnsubscribeEvents relay_actor_id={} — forwarder task aborted",
                self.session_id, msg.relay_actor_id
            );
        } else {
            info!(
                "Session {}: UnsubscribeEvents relay_actor_id={} had no registered forwarder",
                self.session_id, msg.relay_actor_id
            );
        }
        Ok(())
    }
}

/// Set planning context on a delegate session.
///
/// Appends the parent session's planning summary to this session's
/// system prompt. Used by the delegation orchestrator to inject context
/// without requiring direct access to the session's `SessionStore`.
impl Message<crate::agent::messages::SetPlanningContext> for SessionActor {
    type Reply = Result<(), AgentError>;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::SetPlanningContext,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        info!(
            "Session {}: SetPlanningContext ({} bytes)",
            self.session_id,
            msg.summary.len()
        );

        // Get current system prompt and append the planning summary
        let mut system_prompt = self.get_session_system_prompt().await;
        system_prompt.push(msg.summary.into());

        // Rebuild the LLM config with the updated system prompt
        let current_config = self
            .config
            .provider
            .history_store()
            .get_session_llm_config(&self.session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;

        if let Some(current) = current_config {
            // Reconstruct full LLMParams from the stored config JSON.
            // This preserves provider-specific custom params (e.g. text_only,
            // n_ctx, flash_attention for llama_cpp) that are stored via
            // #[serde(flatten)] on LLMParams.custom.  Previously this built
            // a fresh LLMParams with only provider/model/system, silently
            // dropping all custom params — causing delegate sessions to lose
            // their inference configuration.
            let mut llm_params = if let Some(ref params_json) = current.params {
                serde_json::from_value::<querymt::LLMParams>(params_json.clone())
                    .unwrap_or_default()
            } else {
                querymt::LLMParams::new()
            };
            // Re-add provider/model (stored in separate columns, stripped from params JSON)
            llm_params = llm_params.provider(&current.provider).model(&current.model);
            // Replace system prompt with updated version (original + planning context)
            llm_params.system = system_prompt;

            let new_config = self
                .config
                .provider
                .history_store()
                .create_or_get_llm_config(&llm_params)
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?;

            self.config
                .provider
                .history_store()
                .set_session_llm_config(&self.session_id, new_config.id)
                .await
                .map_err(|e| AgentError::Internal(e.to_string()))?;
        }

        Ok(())
    }
}

// ── File Proxy ───────────────────────────────────────────────────────────

impl Message<crate::agent::messages::GetFileIndex> for SessionActor {
    type Reply = Result<
        crate::agent::file_proxy::GetFileIndexResponse,
        crate::agent::file_proxy::FileProxyError,
    >;

    async fn handle(
        &mut self,
        _msg: crate::agent::messages::GetFileIndex,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::agent::file_proxy::{FileProxyError, GetFileIndexResponse};

        let handle = self
            .runtime
            .workspace_handle
            .get()
            .ok_or(FileProxyError::IndexNotReady)?;
        let index = handle.file_index().ok_or(FileProxyError::IndexNotReady)?;
        Ok(GetFileIndexResponse {
            files: index.files.clone(),
            generated_at: index.generated_at,
            workspace_root: index.root.display().to_string(),
        })
    }
}

impl Message<crate::agent::messages::ReadRemoteFile> for SessionActor {
    type Reply = Result<
        crate::agent::file_proxy::ReadRemoteFileResponse,
        crate::agent::file_proxy::FileProxyError,
    >;

    async fn handle(
        &mut self,
        msg: crate::agent::messages::ReadRemoteFile,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::agent::file_proxy::{FileProxyError, ReadRemoteFileResponse};
        use crate::tools::builtins::read_shared::render_read_output;

        let cwd = self
            .runtime
            .cwd
            .as_ref()
            .ok_or(FileProxyError::NoWorkingDirectory)?;

        let path = std::path::Path::new(&msg.path);
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };

        let resolved = joined
            .canonicalize()
            .map_err(|e| FileProxyError::PathResolution(e.to_string()))?;

        let root = querymt_agent_index::resolve_workspace_root(cwd);
        if !resolved.starts_with(&root) {
            return Err(FileProxyError::PathOutsideWorkspace(
                resolved.display().to_string(),
            ));
        }

        let blocks = render_read_output(&resolved, msg.offset, msg.limit)
            .await
            .map_err(FileProxyError::ReadError)?;

        Ok(ReadRemoteFileResponse::from_blocks(blocks))
    }
}

#[cfg(feature = "remote")]
pub(crate) struct GetPeerFileIndex;

#[cfg(feature = "remote")]
impl Message<GetPeerFileIndex> for SessionActor {
    type Reply = Result<
        crate::agent::file_proxy::GetFileIndexResponse,
        crate::agent::file_proxy::FileProxyError,
    >;

    async fn handle(
        &mut self,
        _: GetPeerFileIndex,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::agent::file_proxy::is_shareable_path;
        match <Self as Message<crate::agent::messages::GetFileIndex>>::handle(
            self,
            crate::agent::messages::GetFileIndex,
            ctx,
        )
        .await
        {
            Ok(mut index) => {
                index
                    .files
                    .retain(|entry| is_shareable_path(std::path::Path::new(&entry.path)));
                Ok(index)
            }
            #[cfg(target_os = "linux")]
            Err(crate::agent::file_proxy::FileProxyError::IndexNotReady) => {
                use crate::agent::file_proxy::{FileProxyError, GetFileIndexResponse};
                let root = self
                    .runtime
                    .cwd
                    .as_deref()
                    .ok_or(FileProxyError::NoWorkingDirectory)?;
                let files = crate::tools::builtins::read_shared::list_confined_workspace(root)
                    .await
                    .map_err(FileProxyError::ReadError)?;
                Ok(GetFileIndexResponse {
                    files,
                    generated_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    workspace_root: root.display().to_string(),
                })
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(feature = "remote")]
pub(crate) struct ReadPeerFile(pub(crate) ReadRemoteFile);

#[cfg(feature = "remote")]
impl Message<ReadPeerFile> for SessionActor {
    type Reply = Result<
        crate::agent::file_proxy::ReadRemoteFileResponse,
        crate::agent::file_proxy::FileProxyError,
    >;

    async fn handle(
        &mut self,
        ReadPeerFile(msg): ReadPeerFile,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        use crate::agent::file_proxy::{FileProxyError, ReadRemoteFileResponse, is_shareable_path};
        use crate::tools::builtins::read_shared::{DEFAULT_READ_LIMIT, render_confined_file};
        use std::path::Path;

        let cwd = self
            .runtime
            .cwd
            .as_deref()
            .ok_or(FileProxyError::NoWorkingDirectory)?;
        // Anchor to the session's workspace, not an ancestor discovered by a
        // fresh scan influenced by files the peer can access.
        let index = self
            .runtime
            .workspace_handle
            .get()
            .and_then(|workspace| workspace.file_index());
        let root = index.as_ref().map_or(cwd, |index| index.root.as_path());
        let supplied = Path::new(&msg.path);
        let joined = if supplied.is_absolute() {
            supplied.to_owned()
        } else {
            cwd.join(supplied)
        };
        let relative = joined
            .strip_prefix(root)
            .map_err(|_| FileProxyError::AccessDenied)?;
        if !is_shareable_path(relative) {
            return Err(FileProxyError::AccessDenied);
        }
        let blocks = render_confined_file(
            root,
            relative,
            msg.offset,
            msg.limit.min(DEFAULT_READ_LIMIT),
        )
        .await
        .map_err(FileProxyError::ReadError)?;
        Ok(ReadRemoteFileResponse::from_blocks(blocks))
    }
}

// ── Prompt (the big one) ─────────────────────────────────────────────────

impl Message<SubmitSessionInput> for SessionActor {
    type Reply = Result<SubmitInputResult, AgentError>;

    async fn handle(
        &mut self,
        msg: SubmitSessionInput,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let SubmitSessionInput {
            input, authority, ..
        } = msg;
        let authority = &authority;
        let actor_ref = ctx.actor_ref();
        authority.check().await?;
        if input.session_id != self.session_id {
            return Err(AgentError::from(TurnControlError::SessionMismatch {
                expected: self.session_id.clone(),
                received: input.session_id,
            }));
        }
        validate_prompt_blocks(&input.prompt)?;
        // Peer idempotency keys are not session-global queue identifiers.
        let input_id = match authority {
            TurnAuthority::Local => input.client_input_id.clone(),
            #[cfg(feature = "remote")]
            TurnAuthority::Peer(_) => None,
        }
        .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Bounded same-actor receipt dedup: a retried keyed submission whose
        // original reply was lost returns the original receipt instead of
        // enqueuing a duplicate turn.
        if let Some(client_input_id) = input.client_input_id.as_deref()
            && let Some(receipt) = self.cached_submit_receipt(client_input_id, authority)
        {
            info!(
                "Session {}: SubmitInput client_input_id={} replayed cached receipt",
                self.session_id, client_input_id
            );
            return Ok(receipt);
        }

        match input.delivery {
            InputDelivery::Steer => {
                if self
                    .run
                    .authority()
                    .is_some_and(|active| !authority.can_control(active))
                {
                    return Err(AgentError::MeshAccessDenied);
                }
                // Steered text would reach the model verbatim; reject built-ins
                // regardless of which path submitted them.
                Builtin::parse_for(&input.prompt, Submission::Steer)?;
                let llm_config = self
                    .config
                    .provider
                    .history_store()
                    .get_session_llm_config(&self.session_id)
                    .await
                    .map_err(|e| AgentError::Internal(e.to_string()))?;
                validate_model_image_support(&input.prompt, llm_config.as_ref())?;
                let run = self
                    .run
                    .active()
                    .ok_or_else(|| AgentError::from(TurnControlError::NoActiveRun))?;
                let expected = input
                    .expected_run_id
                    .ok_or_else(|| AgentError::from(TurnControlError::ExpectedRunIdRequired))?;
                // A non-UUID expected id never matches the active run.
                let expected = querymt_agent_core::model::TurnId::from_stored(&expected);
                if expected != run.run_id {
                    return Err(AgentError::from(TurnControlError::RunMismatch {
                        expected,
                        active: run.run_id,
                    }));
                }
                if run.phase == RunPhase::Closing {
                    return Err(AgentError::from(TurnControlError::RunClosing {
                        run_id: run.run_id,
                    }));
                }
                authority.check().await?;
                let blocks = input.prompt.clone();
                let position = run
                    .steering
                    .push(
                        input_id.clone(),
                        input.client_input_id.clone(),
                        input.prompt,
                    )
                    .await
                    .map_err(AgentError::from)?;
                let accepted_at_ms = now_ms();
                let position = crate::agent::utils::u32_from_usize(
                    position,
                    "steering_position",
                    Some(&self.session_id),
                );
                self.config.emit_event(
                    &self.session_id,
                    AgentEventKind::SteeringAccepted {
                        run_id: run.run_id.to_string(),
                        input_id: input_id.clone(),
                        position,
                        blocks,
                        accepted_at_ms: Some(accepted_at_ms),
                    },
                );
                let result = SubmitInputResult::Steered {
                    run_id: run.run_id.to_string(),
                    input_id,
                    position,
                };
                self.record_submit_receipt(input.client_input_id, &result, authority.clone());
                self.publish_runtime().await;
                Ok(result)
            }
            InputDelivery::Queue => {
                // Reject up front rather than issuing a receipt for input that
                // would fail when its turn starts.
                Builtin::parse_for(&input.prompt, Submission::Turn)?;
                let blocks = input.prompt.clone();
                let req =
                    crate::acp::protocol::PromptRequest::new(self.session_id.clone(), input.prompt);
                if !self.run.is_idle() && self.queued_prompts.len() >= MAX_QUEUED_PROMPTS {
                    return Err(queued_prompt_capacity_error(&self.session_id));
                }
                if self.run.is_idle() {
                    self.acquire_run_lease().await?;
                }
                let (reply, receiver) = oneshot::channel();
                let log_session_id = self.session_id.clone();
                let log_input_id = input_id.clone();
                tokio::spawn(async move {
                    match receiver.await {
                        Ok(Err(error)) => warn!(
                            "Session {}: queued input {} execution failed: {}",
                            log_session_id, log_input_id, error
                        ),
                        Err(error) => warn!(
                            "Session {}: queued input {} result channel dropped: {}",
                            log_session_id, log_input_id, error
                        ),
                        Ok(Ok(_)) => {}
                    }
                });
                if self.run.is_idle() {
                    self.start_run(
                        ExecutionOrigin::Interactive,
                        req,
                        Completion {
                            reply,
                            queued_input_id: Some(input_id.clone()),
                        },
                        None,
                        actor_ref.clone(),
                        authority.clone(),
                    );
                    let run_id = self
                        .run
                        .active()
                        .map(|run| run.run_id.to_string())
                        .unwrap_or_default();
                    let result = SubmitInputResult::Started { run_id, input_id };
                    self.record_submit_receipt(input.client_input_id, &result, authority.clone());
                    self.publish_runtime().await;
                    Ok(result)
                } else {
                    let position = self.queued_prompts.len() + 1;
                    let position = crate::agent::utils::u32_from_usize(
                        position,
                        "queued_input_position",
                        Some(&self.session_id),
                    );
                    self.config
                        .emit_event_persisted(
                            &self.session_id,
                            AgentEventKind::InputQueued {
                                input_id: input_id.clone(),
                                position,
                                blocks,
                                accepted_at_ms: Some(now_ms()),
                            },
                        )
                        .await
                        .map_err(AgentError::from)?;
                    self.queued_prompts.push_back(QueuedPrompt {
                        input_id: input_id.clone(),
                        authority: authority.clone(),
                        req,
                        reply,
                    });
                    let result = SubmitInputResult::Queued { input_id, position };
                    self.record_submit_receipt(input.client_input_id, &result, authority.clone());
                    self.publish_runtime().await;
                    Ok(result)
                }
            }
        }
    }
}

impl Message<DiscardQueuedInput> for SessionActor {
    type Reply = Result<DiscardQueuedInputResult, AgentError>;

    async fn handle(
        &mut self,
        msg: DiscardQueuedInput,
        _: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let DiscardQueuedInput {
            input_id,
            authority,
        } = msg;
        authority.check().await?;
        let Some(position) = self.queued_prompts.iter().position(|queued| {
            queued.input_id == input_id && authority.can_control(&queued.authority)
        }) else {
            return Ok(DiscardQueuedInputResult::NotPending { input_id });
        };
        self.config
            .emit_event_persisted(
                &self.session_id,
                AgentEventKind::QueuedInputDiscarded {
                    input_id: input_id.clone(),
                    reason: "removed_by_user".to_string(),
                },
            )
            .await
            .map_err(AgentError::from)?;
        let Some(queued) = self.queued_prompts.remove(position) else {
            return Err(AgentError::Internal(format!(
                "queued input {input_id} disappeared after discard persistence"
            )));
        };
        let _ = queued
            .reply
            .send(Ok(PromptResponse::new(StopReason::Cancelled)));
        self.publish_runtime().await;
        Ok(DiscardQueuedInputResult::Discarded { input_id })
    }
}

impl Message<Prompt> for SessionActor {
    type Reply = DelegatedReply<Result<PromptResponse, AgentError>>;

    async fn handle(&mut self, msg: Prompt, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        let queued = self.enqueue_prompt(msg, ctx.actor_ref()).await;
        ctx.spawn(wait_for_prompt_reply(queued))
    }
}

type PromptReceiver = oneshot::Receiver<Result<PromptResponse, AgentError>>;

async fn wait_for_prompt_reply(
    queued: Result<PromptReceiver, AgentError>,
) -> Result<PromptResponse, AgentError> {
    queued?
        .await
        .map_err(|_| AgentError::Internal("prompt execution dropped".into()))?
}

impl SessionActor {
    async fn enqueue_prompt(
        &mut self,
        msg: Prompt,
        actor_ref: &kameo::actor::ActorRef<Self>,
    ) -> Result<PromptReceiver, AgentError> {
        let authority = msg.authority;
        authority.check().await?;
        if msg.req.session_id.0.as_ref() != self.session_id {
            return Err(TurnControlError::SessionMismatch {
                expected: self.session_id.clone(),
                received: msg.req.session_id.0.to_string(),
            }
            .into());
        }
        validate_prompt_blocks(&msg.req.prompt)?;
        if self.run.is_idle() {
            self.acquire_run_lease().await?;
        }
        let (reply, receiver) = oneshot::channel();
        if !self.run.is_idle() {
            if self.queued_prompts.len() >= MAX_QUEUED_PROMPTS {
                return Err(queued_prompt_capacity_error(&self.session_id));
            }
            let input_id = Uuid::new_v4().to_string();
            let position = self.queued_prompts.len() + 1;
            let blocks = msg.req.prompt.clone();
            self.queued_prompts.push_back(QueuedPrompt {
                input_id: input_id.clone(),
                authority,
                req: msg.req,
                reply,
            });
            self.config.emit_event(
                &self.session_id,
                AgentEventKind::InputQueued {
                    input_id,
                    position: crate::agent::utils::u32_from_usize(
                        position,
                        "legacy_queued_input_position",
                        Some(&self.session_id),
                    ),
                    blocks,
                    accepted_at_ms: Some(now_ms()),
                },
            );
        } else {
            self.start_run(
                ExecutionOrigin::Interactive,
                msg.req,
                Completion {
                    reply,
                    queued_input_id: None,
                },
                None,
                actor_ref.clone(),
                authority,
            );
        }
        self.publish_runtime().await;
        Ok(receiver)
    }
}

// ── ScheduledPrompt ──────────────────────────────────────────────────────
//
// Fired by SchedulerActor. Similar to Prompt but:
// 1. Builds a PromptRequest from the schedule's prompt_text.
// 2. Sets ExecutionOrigin::Scheduled on the execution context.
// 3. Emits ScheduledExecutionCompleted / ScheduledExecutionFailed terminal
//    events so the SchedulerActor can correlate cycle outcomes.

impl TurnAuthority {
    pub(crate) async fn from_stored_origin(
        origin: crate::session::domain_schedule::ScheduleOrigin,
        #[cfg(feature = "remote")] mesh: Option<&querymt_remote::MeshHandle>,
    ) -> Result<TurnAuthority, AgentError> {
        use crate::session::domain_schedule::ScheduleOrigin;
        match origin {
            ScheduleOrigin::Local => Ok(TurnAuthority::Local),
            #[cfg(feature = "remote")]
            ScheduleOrigin::Peer {
                host,
                peer,
                protocol,
            } => {
                use crate::session::domain_schedule::SchedulePeerProtocol;
                use querymt_remote::{
                    NodeId,
                    peer_access::{PeerGrantKey, SharingProtocol},
                };

                let mesh = mesh.ok_or(AgentError::MeshAccessDenied)?;
                let store = mesh
                    .quota_store()
                    .ok_or(AgentError::MeshAccessDenied)?
                    .clone();
                let host = NodeId::parse(&host).map_err(|_| AgentError::MeshAccessDenied)?;
                if host.as_peer_id() != mesh.peer_id() {
                    return Err(AgentError::MeshAccessDenied);
                }
                let peer = NodeId::parse(&peer).map_err(|_| AgentError::MeshAccessDenied)?;
                let key = PeerGrantKey { host, peer };
                let changes = parking_lot::Mutex::new(store.subscribe_changes());
                let state = store
                    .grant(&key)
                    .await
                    .map_err(|_| AgentError::MeshAccessDenied)?;
                Ok(TurnAuthority::Peer(Arc::new(PeerTurn {
                    key,
                    pinned: state.grant,
                    changes,
                    protocol: match protocol {
                        SchedulePeerProtocol::Legacy => SharingProtocol::Legacy,
                        SchedulePeerProtocol::V1 => SharingProtocol::V1,
                    },
                    store,
                    pass: None,
                })))
            }
            #[cfg(not(feature = "remote"))]
            ScheduleOrigin::Peer { .. } => Err(AgentError::MeshAccessDenied),
        }
    }
}

impl SessionActor {
    fn emit_scheduled_admission_failure(&self, msg: &ScheduledPrompt, error: &AgentError) {
        self.config.emit_event(
            &self.session_id,
            AgentEventKind::ScheduledExecutionFailed {
                schedule_public_id: msg.schedule_public_id.clone(),
                turn_id: Some(msg.turn_id.to_string()),
                error: error.to_string(),
            },
        );
    }
}

impl Message<ScheduledPrompt> for SessionActor {
    type Reply = DelegatedReply<Result<PromptResponse, AgentError>>;

    async fn handle(
        &mut self,
        msg: ScheduledPrompt,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        debug!(
            "Session {}: ScheduledPrompt received (schedule={})",
            self.session_id, msg.schedule_public_id
        );

        #[cfg(feature = "remote")]
        let current_mesh = self.config.provider.mesh_handle();
        let authority = match TurnAuthority::from_stored_origin(
            msg.origin.clone(),
            #[cfg(feature = "remote")]
            current_mesh.as_deref().or(self.mesh.as_ref()),
        )
        .await
        {
            Ok(authority) => match authority.check().await {
                Ok(()) => Ok(authority),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let authority = match authority {
            Ok(authority) => authority,
            Err(error) => {
                self.emit_scheduled_admission_failure(&msg, &error);
                return ctx.spawn(async move { Err(error) });
            }
        };

        if !self.run.is_idle() {
            let session_id = self.session_id.clone();
            let schedule_public_id = msg.schedule_public_id.clone();
            return ctx.spawn(async move {
                Err(AgentError::AdmissionRejected {
                    reason: format!(
                        "session {session_id} already has an active run; scheduled execution {schedule_public_id} must be retried"
                    ),
                })
            });
        }

        // Rejected before any run state exists. The scheduler observes the
        // failed cycle via `ScheduledExecutionFailed`. That event reaches the
        // scheduler as a `ProcessEvent` message, which its mailbox handles only
        // after `fire_schedule` has registered the cycle and its timeout, so the
        // failure removes the cycle and aborts the timeout. Terminal handling is
        // idempotent per (schedule, turn), so a racing timeout is dropped.
        if let Err(error) = self.acquire_run_lease().await {
            self.emit_scheduled_admission_failure(&msg, &error);
            return ctx.spawn(async move { Err(error) });
        }

        // Build a PromptRequest from the schedule's prompt text
        let prompt_request = crate::acp::protocol::PromptRequest::new(
            crate::acp::protocol::SessionId::from(self.session_id.clone()),
            vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new(msg.prompt_text),
            )],
        );
        let (reply, receiver) = oneshot::channel();
        self.start_run(
            ExecutionOrigin::Scheduled {
                schedule_public_id: msg.schedule_public_id,
                task_public_id: msg.task_public_id,
            },
            prompt_request,
            Completion {
                reply,
                queued_input_id: None,
            },
            Some(msg.turn_id),
            ctx.actor_ref().clone(),
            authority,
        );
        ctx.spawn(async move {
            receiver.await.unwrap_or_else(|_| {
                Err(AgentError::Internal(
                    "scheduled execution dropped".to_string(),
                ))
            })
        })
    }
}

/// Tell the session actor what a run reports, one message per change the
/// forwarder sees. Changes that arrive while a message is being handled
/// coalesce, so the actor always ends with the latest phase. Ends when the run
/// drops its sender or the actor is gone.
async fn forward_run_signals(
    mut signals: tokio::sync::watch::Receiver<RunSignal>,
    actor: kameo::actor::ActorRef<SessionActor>,
    generation: u64,
    run_id: querymt_agent_core::model::TurnId,
) {
    let mut last_phase = RunSignal::starting().phase;
    while signals.changed().await.is_ok() {
        let signal = *signals.borrow_and_update();
        // A phase is sent only when it moved, so a revision bump does not
        // undo a phase the actor set itself (a cancelled run is `Closing`).
        let phase = (signal.phase != last_phase).then_some(signal.phase);
        last_phase = signal.phase;
        if actor
            .tell(RunPhaseChanged {
                generation,
                run_id,
                phase,
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

/// Execute a prompt in a detached task (called from `ctx.spawn()`).
///
/// This function gathers all needed state upfront and runs the full execution cycle.
/// It does NOT access the actor — everything is passed as parameters.
struct DetachedPromptExecution {
    req: crate::acp::protocol::PromptRequest,
    session_id: String,
    runtime: Arc<SessionRuntime>,
    config: Arc<AgentConfig>,
    cancel_token: CancellationToken,
    mode: AgentMode,
    tool_config: ToolConfig,
    /// Origin of this execution — interactive or scheduled.
    execution_origin: ExecutionOrigin,
    /// The run's single turn id; the `String` run id used by events, hooks and
    /// steering is derived from it.
    turn_id: querymt_agent_core::model::TurnId,
    steering: Arc<crate::agent::turn_control::SteeringInbox>,
    phase_reporter: tokio::sync::watch::Sender<RunSignal>,
}

enum PermitWaitOutcome {
    Acquired(OwnedSemaphorePermit),
    TimedOut,
    Cancelled,
}

async fn acquire_execution_permit_with_timeout(
    permit: &Arc<tokio::sync::Semaphore>,
    cancel_token: &CancellationToken,
    timeout: std::time::Duration,
) -> Result<PermitWaitOutcome, AgentError> {
    tokio::select! {
        permit = tokio::time::timeout(timeout, permit.clone().acquire_owned()) => match permit {
            Ok(result) => result
                .map(PermitWaitOutcome::Acquired)
                .map_err(|_| AgentError::SessionSemaphoreClosed),
            Err(_) => Ok(PermitWaitOutcome::TimedOut),
        },
        _ = cancel_token.cancelled() => Ok(PermitWaitOutcome::Cancelled),
    }
}

fn normalized_run_instruction(prompt: &[crate::acp::protocol::ContentBlock]) -> String {
    render_prompt_for_display(prompt)
}

/// Fixed-root editor integrations cannot serve a workspace-bound session:
/// the client bridge resolves language queries against the editor's opened
/// checkout, not this session's isolated managed workspace. Disable the
/// bridge with an explicit diagnostic instead of silently serving the
/// original checkout; the `language_query` tool then rejects with a clear
/// error for such sessions.
fn editor_bridge_for_workspace(
    workspace_bound: bool,
    bridge: Option<ClientBridgeSender>,
) -> Option<ClientBridgeSender> {
    if !workspace_bound {
        return bridge;
    }
    if bridge.is_some() {
        warn!(
            "editor workspace-query bridge disabled: the session runs in an \
             isolated managed workspace, but the editor client is bound to a \
             different checkout; language queries are rejected for this session"
        );
    }
    None
}

fn validate_model_image_support(
    prompt: &[crate::acp::protocol::ContentBlock],
    llm_config: Option<&LLMConfig>,
) -> Result<(), AgentError> {
    if !prompt_contains_images(prompt) {
        return Ok(());
    }
    let Some(llm_config) = llm_config else {
        return Ok(());
    };
    if !model_supports_images(
        querymt_agent_services::model_info::get_model_info(&llm_config.provider, &llm_config.model)
            .as_ref(),
    ) {
        return Err(AgentError::InvalidPromptContent {
            message: format!(
                "selected model {}/{} does not support image attachments",
                llm_config.provider, llm_config.model
            ),
        });
    }
    Ok(())
}

fn model_supports_images(model_info: Option<&querymt::providers::ModelInfo>) -> bool {
    model_info.is_none_or(|info| {
        info.capabilities.attachment
            || info
                .capabilities
                .modalities
                .input
                .iter()
                .any(|modality| modality == "image")
    })
}

async fn validate_execution_task(
    store: &dyn crate::session::store::SessionStore,
    execution_origin: &ExecutionOrigin,
    session_id: &str,
) -> Result<Option<crate::session::domain::Task>, AgentError> {
    let ExecutionOrigin::Scheduled { task_public_id, .. } = execution_origin else {
        return Ok(None);
    };

    let task = store
        .get_task_for_session(session_id, task_public_id)
        .await
        .map_err(AgentError::from)?
        .ok_or_else(|| {
            AgentError::Internal(format!(
                "scheduled task not found in session: {task_public_id}"
            ))
        })?;
    if task.status != crate::session::domain::TaskStatus::Active {
        return Err(AgentError::Internal(format!(
            "scheduled task is not active: {task_public_id}"
        )));
    }
    Ok(Some(task))
}

/// A prompt's ACP response plus the QueryMT stop that ended it, if any.
///
/// The stop travels to the actor for `run_completed`; the response keeps only
/// the ACP stop reason.
struct PromptRun {
    response: PromptResponse,
    stop: Option<(querymt_agent_core::events::StopType, String)>,
}

impl PromptRun {
    fn finished(response: PromptResponse) -> Self {
        Self {
            response,
            stop: None,
        }
    }
}

/// How a finished prompt task ended, for `run_completed`.
fn run_disposition(
    cancelled: bool,
    result: &Result<PromptRun, AgentError>,
) -> RunCompletionDisposition {
    if cancelled {
        return RunCompletionDisposition::Cancelled;
    }
    match result {
        Err(_) => RunCompletionDisposition::Failed,
        Ok(PromptRun {
            stop: Some((stop_type, reason)),
            ..
        }) => RunCompletionDisposition::Stopped {
            stop_type: *stop_type,
            reason: reason.clone(),
        },
        Ok(_) => RunCompletionDisposition::Completed,
    }
}

/// The text of a panic payload: what `panic!` was given, when that was a
/// string.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

/// [`execute_authorized_prompt`], with a panic of the run turned into the run's
/// failure.
///
/// Everything that ends a run (the reply, the scheduled terminal events and
/// `PromptFinished`) happens after the execution returns. A panic that
/// escaped would end the task first and leave the run slot, the run lease and
/// the queued prompts waiting for a run that is gone.
async fn execute_prompt_guarded(
    exec: DetachedPromptExecution,
    authority: &TurnAuthority,
    config: &AgentConfig,
    session_id: &str,
    run_id: querymt_agent_core::model::TurnId,
) -> Result<PromptRun, AgentError> {
    use futures_util::FutureExt as _;

    match std::panic::AssertUnwindSafe(execute_authorized_prompt(exec, authority))
        .catch_unwind()
        .await
    {
        Ok(run) => run,
        Err(payload) => {
            let message = format!("run panicked: {}", panic_message(payload.as_ref()));
            error!("Session {session_id}: run {run_id} {message}");
            config.emit_event(
                session_id,
                AgentEventKind::Error {
                    message: message.clone(),
                },
            );
            Err(AgentError::Internal(message))
        }
    }
}

#[cfg(test)]
async fn execute_prompt_detached(exec: DetachedPromptExecution) -> Result<PromptRun, AgentError> {
    execute_authorized_prompt(exec, &TurnAuthority::Local).await
}

async fn execute_authorized_prompt(
    exec: DetachedPromptExecution,
    authority: &TurnAuthority,
) -> Result<PromptRun, AgentError> {
    authority.check().await?;
    #[cfg(feature = "remote")]
    if let TurnAuthority::Peer(peer) = authority {
        let session = exec
            .config
            .provider
            .with_session(&exec.session_id)
            .await
            .map_err(|error| AgentError::Internal(error.to_string()))?;
        let binding = peer
            .host_binding(&session)
            .map(|(provider, model)| (provider.to_string(), model.to_string()));
        let binding = binding
            .as_ref()
            .map(|(provider, model)| (provider.as_str(), model.as_str()));
        peer.grant_for(binding).await?;
        let cancel = exec.cancel_token.clone();
        let run = execute_prompt_with_span(exec, authority, Some(session));
        tokio::pin!(run);
        return tokio::select! {
            biased;
            error = peer.revoked(binding) => {
                // Let the normal cancellation path finish accounting and cleanup.
                cancel.cancel();
                let _ = run.await;
                Err(error)
            }
            result = &mut run => {
                peer.grant_for(binding).await?;
                result
            }
        };
    }
    execute_prompt_with_span(exec, authority, None).await
}

async fn execute_prompt_with_span(
    exec: DetachedPromptExecution,
    authority: &TurnAuthority,
    pinned_session: Option<crate::session::provider::SessionHandle>,
) -> Result<PromptRun, AgentError> {
    // Construct the existing execution span explicitly: OTel 0.33 starts it on entry,
    // so the request's remote parent must be installed before instrumenting the future.
    let span = info_span!(
        parent: None,
        "agent.prompt.execute",
        otel.name = "invoke_agent",
        otel.kind = "internal",
        gen_ai.operation.name = "invoke_agent",
        gen_ai.conversation.id = %exec.session_id,
        session.id = %exec.session_id,
        session_id = %exec.session_id,
        execution_origin = ?exec.execution_origin,
        mode = %exec.mode,
        cwd = tracing::field::display(
            exec.runtime
                .cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "none".to_string())
        ),
        admission_ms = tracing::field::Empty,
        intent_projection_ms = tracing::field::Empty,
    );
    crate::agent::utils::genai::agent_id(&span, exec.config.provider.agent_id.as_deref());
    // Detached/queued work owns its request parent, never the actor's long-lived span.
    if let Some(parent) = exec
        .req
        .meta
        .as_ref()
        .and_then(crate::acp::trace_context::extract_acp_trace_context_from_meta)
    {
        let _ = span.set_parent(parent);
    }
    async move {
        execute_prompt_inner(exec, authority, pinned_session)
            .await
            .inspect_err(|error| {
                let error_type = match error {
                    AgentError::PermissionCancelled => return,
                    AgentError::ClientBridgeClosed
                    | AgentError::PermissionChannelDropped
                    | AgentError::WorkspaceQueryChannelDropped => "client_disconnected",
                    AgentError::SessionTimeout { .. } => "timeout",
                    AgentError::InvalidPromptContent { .. } => "invalid_prompt",
                    _ => "agent_error",
                };
                genai::error(error_type);
            })
    }
    .instrument(span)
    .await
}

async fn execute_prompt_inner(
    exec: DetachedPromptExecution,
    authority: &TurnAuthority,
    pinned_session: Option<crate::session::provider::SessionHandle>,
) -> Result<PromptRun, AgentError> {
    let admission_started = std::time::Instant::now();
    let DetachedPromptExecution {
        req,
        session_id,
        runtime,
        config,
        cancel_token,
        mode,
        tool_config,
        execution_origin,
        turn_id,
        steering,
        phase_reporter,
    } = exec;
    let run_id = turn_id;
    debug!(
        "Prompt request for session {} with {} block(s)",
        session_id,
        req.prompt.len()
    );
    let client_prompt_id = client_prompt_id_from_meta(req.meta.as_ref());
    // One parser recognises every built-in. Native `/worktree` administration
    // is intercepted before a turn starts, so it never reaches this scheduled
    // or queued path.
    let builtin = Builtin::parse_for(&req.prompt, Submission::Turn)?;
    validate_prompt_blocks(&req.prompt)?;

    // Acquire execution permit (blocking with timeout)
    let _permit = match acquire_execution_permit_with_timeout(
        &runtime.execution_permit,
        &cancel_token,
        std::time::Duration::from_millis(100),
    )
    .await?
    {
        PermitWaitOutcome::Acquired(permit) => permit,
        PermitWaitOutcome::Cancelled => {
            return Ok(PromptRun::finished(PromptResponse::new(
                StopReason::Cancelled,
            )));
        }
        PermitWaitOutcome::TimedOut => {
            debug!(
                "Session {} is busy, waiting for previous operation to complete...",
                session_id
            );
            config.emit_event(
                &session_id,
                AgentEventKind::SessionQueued {
                    reason: "waiting for previous operation to complete".to_string(),
                },
            );
            let timeout_duration = std::time::Duration::from_secs(config.execution_timeout_secs);
            match acquire_execution_permit_with_timeout(
                &runtime.execution_permit,
                &cancel_token,
                timeout_duration,
            )
            .await?
            {
                PermitWaitOutcome::Acquired(permit) => permit,
                PermitWaitOutcome::Cancelled => {
                    return Ok(PromptRun::finished(PromptResponse::new(
                        StopReason::Cancelled,
                    )));
                }
                PermitWaitOutcome::TimedOut => {
                    return Err(AgentError::SessionTimeout {
                        details: format!(
                            "session_id={}, timeout={}s",
                            session_id, config.execution_timeout_secs
                        ),
                    });
                }
            }
        }
    };

    authority.check().await?;
    let history_store = config.provider.history_store();
    let scheduled_task =
        validate_execution_task(&*history_store, &execution_origin, &session_id).await?;
    #[cfg(feature = "remote")]
    let peer_grant = match authority {
        TurnAuthority::Local => None,
        TurnAuthority::Peer(peer) => {
            if cancel_token.is_cancelled() {
                return Ok(PromptRun::finished(PromptResponse::new(
                    StopReason::Cancelled,
                )));
            }
            let binding = pinned_session
                .as_ref()
                .and_then(|session| peer.host_binding(session));
            Some((peer, peer.grant_for(binding).await?))
        }
    };
    #[cfg(feature = "remote")]
    let restricted = peer_grant
        .as_ref()
        .is_some_and(|(_, grant)| *grant != querymt_remote::peer_access::PeerGrant::Full);
    #[cfg(not(feature = "remote"))]
    let restricted = false;
    #[cfg(feature = "remote")]
    let filesystem = peer_grant
        .as_ref()
        .is_some_and(|(peer, grant)| grant.allows_filesystem(peer.protocol));
    #[cfg(not(feature = "remote"))]
    let filesystem = false;
    // Unclassified owner hooks can implement mandatory admission rules, so do
    // not silently bypass them for a restricted turn.
    if restricted
        && (builtin.is_some()
            || config.hooks.is_enabled()
            || config
                .middleware_drivers
                .iter()
                .any(|driver| !driver.supports_restricted_mesh()))
    {
        return Err(AgentError::MeshAccessDenied);
    }

    // Clean up revert state if a new prompt is sent while in reverted state.
    // Also prunes the event journal so undone turns don't reappear on reload.
    // IMPORTANT: this must run BEFORE loading RuntimeContext so that
    // `get_history` / `load_working_context` only sees the surviving messages.
    if !restricted
        && let Err(e) = crate::agent::undo::cleanup_revert_on_prompt(
            &config.provider,
            &**config.event_sink.journal(),
            Some(&config.event_sink.session_stream(&session_id)),
            &session_id,
            Some(&run_id.to_string()),
        )
        .await
    {
        warn!("Failed to clean up revert state: {}", e);
    }

    // Get Session Context (turn-pinned)
    let session_handle = match pinned_session {
        Some(session) => session,
        None => config
            .provider
            .with_session(&session_id)
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?,
    };

    #[cfg(feature = "remote")]
    let session_handle = if let Some((peer, grant)) = peer_grant {
        session_handle.for_mesh_turn(peer, &grant, config.fallback_context_tokens())?
    } else {
        session_handle
    };

    // Slash-command expansion can alter text, but never attachment capability.
    validate_model_image_support(&req.prompt, session_handle.llm_config())?;

    // Create and load RuntimeContext
    let mut runtime_context =
        RuntimeContext::new(config.provider.history_store(), session_id.clone())
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?;
    runtime_context
        .load_working_context()
        .await
        .map_err(|e| AgentError::Internal(e.to_string()))?;

    // Session accounting is hydrated once per runtime and retained across
    // cycles; each cycle gets its own immutable handle with zero cycle spend.
    // Failing to load it is an execution error: paid work must not proceed
    // without a ledger.
    let session_accounting = runtime
        .accounting(&session_id, &config.event_sink)
        .await
        .map_err(|e| AgentError::Internal(format!("failed to load accounting: {e}")))?;
    let cycle_accounting =
        querymt_agent_services::accounting::CycleAccounting::begin(session_accounting);

    // The run's read-only inputs, built into a `TurnEnv` once the run's model
    // is resolved. For a prompt that is after the prompt is stored, so a
    // provider failure leaves the prompt in the history; for native `/compact`
    // it is before the compaction starts. The closure is the one place the
    // struct literal is written, so a missing field is a compile error, and it
    // runs once, on the path that gets a model. The cancellation token
    // propagates into individual tool calls for cooperative cancellation; the
    // knowledge store lets knowledge tools reach it.
    let tool_set = TurnEnv::build_tool_set(&config, &runtime, &tool_config);
    let workspace_query_bridge = editor_bridge_for_workspace(
        runtime.managed_checkout.get().copied().unwrap_or(false),
        config.workspace_query_bridge_for_session(&session_id),
    );
    let make_env = |model: ResolvedModel| TurnEnv {
        session_id: session_id.clone(),
        turn_id,
        runtime: runtime.clone(),
        session_handle: session_handle.clone(),
        model,
        turn_authority: authority.clone(),
        restricted_mesh_turn: restricted,
        confined_mesh_read: restricted
            && filesystem
            && cfg!(all(feature = "remote", target_os = "linux")),
        cancellation_token: cancel_token.clone(),
        tool_config: tool_config.clone(),
        knowledge_store: config.knowledge_store(),
        repository_knowledge: config.repository_knowledge(),
        event_sink: Some(config.event_sink.clone()),
        workspace_query_bridge,
        turn_mode: mode,
        steering: Some(steering),
        phase_reporter: Some(phase_reporter),
        cycle_accounting,
        tool_set,
    };
    let mut turn = Turn::new(runtime_context);

    if let Some(task) = &scheduled_task {
        turn.state.active_task = Some(task.clone());
    }

    // The run reads the stored history once, here. From now on it is the only
    // writer of its session's conversation, so it holds what the store holds.
    turn.history = match crate::agent::execution::turn::History::load(&session_handle).await {
        Ok(history) => history,
        Err(error) => {
            config.emit_event(
                &session_id,
                AgentEventKind::Error {
                    message: error.to_string(),
                },
            );
            return Err(AgentError::Internal(error.to_string()));
        }
    };

    // 3b. Native `/compact [instructions]`: compact now, no model turn.
    // Built-ins are matched before registry expansion so they take priority
    // over user-defined commands of the same name.
    if let Some(command) = builtin
        .as_ref()
        .filter(|command| command.builtin == crate::slash_commands::builtin::Builtin::Compact)
    {
        authority.check().await?;
        log::info!("Session {}: manual compaction requested", session_id);
        let instructions = (!command.args.is_empty()).then_some(command.args.as_str());
        // The model is resolved first, without a retry (its capabilities size
        // the estimate), but a provider that cannot be built does not end the
        // command yet: the `pre_compaction` hook runs before anything needs the
        // session's provider and may supply the summary, and the compaction
        // policy may send the request to another provider. The failure is
        // reported once the summary has to come from the provider that failed.
        // An empty conversation needs no model: there is nothing to compact.
        let outcome: Result<crate::agent::execution::CompactionOutcome, TurnError> =
            if turn.history.as_slice().is_empty() {
                Ok(crate::agent::execution::CompactionOutcome::NothingToCompact)
            } else {
                let (model, unresolved) =
                    match crate::agent::execution::resolve_model_once(&session_handle).await {
                        Ok(model) => (model, None),
                        Err(error) => (ResolvedModel::unavailable(error.to_string()), Some(error)),
                    };
                let env = make_env(model);
                crate::agent::execution::compact_history_with(
                    &config,
                    &env,
                    &mut turn,
                    crate::agent::execution::TRIGGER_MANUAL,
                    instructions,
                    unresolved,
                )
                .await
                .map_err(TurnError::from)
            };
        return match outcome {
            Ok(crate::agent::execution::CompactionOutcome::Completed { .. }) => Ok(
                PromptRun::finished(PromptResponse::new(StopReason::EndTurn)),
            ),
            Ok(crate::agent::execution::CompactionOutcome::Blocked { message }) => {
                config.emit_event(
                    &session_id,
                    AgentEventKind::HookNotice {
                        event_name: "PreCompaction".to_string(),
                        message,
                        is_error: false,
                    },
                );
                Ok(PromptRun::finished(PromptResponse::new(
                    StopReason::EndTurn,
                )))
            }
            Ok(crate::agent::execution::CompactionOutcome::PriceLimit { message }) => {
                Ok(PromptRun {
                    response: PromptResponse::new(StopReason::from(
                        querymt_agent_core::events::StopType::PriceLimit,
                    )),
                    stop: Some((querymt_agent_core::events::StopType::PriceLimit, message)),
                })
            }
            Ok(crate::agent::execution::CompactionOutcome::NothingToCompact) => {
                config.emit_event(
                    &session_id,
                    AgentEventKind::HookNotice {
                        event_name: "Compact".to_string(),
                        message: "Nothing to compact: the conversation is empty".to_string(),
                        is_error: false,
                    },
                );
                Ok(PromptRun::finished(PromptResponse::new(
                    StopReason::EndTurn,
                )))
            }
            Err(error) => {
                let message = error.to_string();
                config.emit_event(
                    &session_id,
                    AgentEventKind::Error {
                        message: message.clone(),
                    },
                );
                Err(
                    provider_failure_from_execution_error(&error, session_handle.llm_config())
                        .unwrap_or(AgentError::Internal(message)),
                )
            }
        };
    }

    // 4. Slash command expansion (before storing user messages)
    let mut req = req;
    // Managed sessions expand against the registry resolved from their
    // bound checkout, matching the advertised catalog; legacy local
    // sessions keep the agent-level registry built at startup.
    let slash_registry = runtime.effective_slash_registry(&config.slash_command_registry);
    if !restricted
        && let Some(crate::acp::protocol::ContentBlock::Text(text_content)) = req.prompt.first()
        && let Some(expansion) =
            crate::slash_commands::try_expand(&text_content.text, slash_registry)
    {
        log::info!(
            "Session {}: expanding slash command '/{}'",
            session_id,
            expansion.invocation.name
        );
        // Replace the first text block with the expanded prompt
        req.prompt[0] = crate::acp::protocol::ContentBlock::Text(
            crate::acp::protocol::TextContent::new(expansion.prompt_text),
        );
    }

    // 5. Store User Messages
    // Keep separate projections for user-visible events vs LLM replay context.
    let display_content = render_prompt_for_display(&req.prompt);

    // User text only for intent snapshot (clean, no attachments)
    let user_text = format_prompt_user_text_only(&req.prompt);

    let message_id = Uuid::new_v4().to_string();

    authority.check().await?;
    let hook_result = if restricted {
        crate::hooks::UserPromptSubmitResult::default()
    } else {
        config
            .hooks
            .run_user_prompt_submit(crate::hooks::UserPromptSubmitRequest {
                session_id: session_id.clone(),
                turn_id: run_id.to_string(),
                cwd: runtime.cwd.as_deref().map(|p| p.to_path_buf()),
                model: session_handle
                    .llm_config()
                    .map(|cfg| cfg.model.clone())
                    .unwrap_or_default(),
                permission_mode: crate::hooks::permission_mode_label(mode).to_string(),
                prompt: user_text.clone(),
            })
            .await
            .map_err(|e| AgentError::Internal(e.to_string()))?
    };
    emit_hook_notices(&config, &session_id, hook_result.notices.clone());
    if hook_result.should_block {
        let reason = hook_result
            .block_reason
            .unwrap_or_else(|| "prompt blocked by hook".to_string());
        config.emit_event(
            &session_id,
            AgentEventKind::Error {
                message: reason.clone(),
            },
        );
        return Err(AgentError::Internal(reason));
    }

    // Native orientation runs after hooks accepted the prompt and before the
    // message is stored, so the first model request already carries the cards.
    // It is additive and fail-open; it never blocks or alters the prompt.
    let mut context_contributions = hook_result.context_contributions;
    if !restricted {
        let orientation = crate::agent::repository_delivery::orientation_contexts(
            &config,
            &session_id,
            runtime.cwd.as_deref(),
            &tool_config,
            &mut turn,
            &user_text,
            &context_contributions,
        )
        .await;
        context_contributions.extend(orientation);
    }
    authority.check().await?;

    let mut parts = vec![MessagePart::Prompt {
        blocks: req.prompt.clone(),
    }];
    parts.extend(
        context_contributions
            .into_iter()
            .map(|context| MessagePart::HookContext {
                event_name: context.event_name,
                handler_id: context.handler_id,
                tool_use_id: context.tool_use_id,
                content: context.content,
            }),
    );
    let agent_msg = AgentMessage {
        id: message_id.clone(),
        session_id: session_id.clone(),
        role: ChatRole::User,
        parts,
        created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        parent_message_id: None,
        source_provider: None,
        source_model: None,
    };

    if let Err(e) = turn.history.commit(&session_handle, vec![agent_msg]).await {
        config.emit_event(
            &session_id,
            AgentEventKind::Error {
                message: e.to_string(),
            },
        );
        return Err(AgentError::Internal(e.to_string()));
    }

    for block in &req.prompt {
        config.emit_event(
            &session_id,
            AgentEventKind::UserPromptBlock {
                message_id: message_id.clone(),
                client_prompt_id: client_prompt_id.clone(),
                block: block.clone(),
            },
        );
    }
    config
        .emit_event_persisted(
            &session_id,
            AgentEventKind::PromptReceived {
                content: display_content.clone(),
                message_id: Some(message_id.clone()),
            },
        )
        .await
        .map_err(|error| AgentError::Internal(error.to_string()))?;
    config.emit_event(
        &session_id,
        AgentEventKind::UserMessageStored {
            content: display_content.clone(),
        },
    );

    let intent_started = std::time::Instant::now();
    if !restricted
        && let Err(error) =
            maybe_update_session_intent(&config, &session_id, &mut turn, &user_text, &message_id)
                .await
    {
        warn!(
            "Failed to update intent projection after storing prompt {}: {}",
            message_id, error
        );
    }
    tracing::Span::current().record(
        "intent_projection_ms",
        intent_started.elapsed().as_millis() as u64,
    );

    let origin_label = match &execution_origin {
        ExecutionOrigin::Interactive => "interactive".to_string(),
        ExecutionOrigin::Scheduled {
            schedule_public_id,
            task_public_id,
        } => format!("scheduled:{schedule_public_id}:{task_public_id}"),
    };
    let objective_task = match &execution_origin {
        ExecutionOrigin::Scheduled { .. } => scheduled_task.as_ref(),
        ExecutionOrigin::Interactive => turn.state.active_task.as_ref(),
    };
    let objective = crate::agent::objective::RunObjective::new(
        run_id,
        normalized_run_instruction(&req.prompt),
        origin_label,
        objective_task,
        turn.state.current_intent.clone(),
    );
    config.emit_event(
        &session_id,
        AgentEventKind::ObjectiveInitialized {
            run_id: run_id.to_string(),
            revision: objective.revision,
            source: "run_prompt".to_string(),
            source_ref: Some(message_id.clone()),
        },
    );
    turn.run_objective = Some(objective);

    config.emit_event(&session_id, AgentEventKind::TurnStarted);

    // The run's model is resolved after the prompt is stored and before its
    // environment exists (and before any hook of the run). A provider that
    // cannot be built fails the run here with the prompt in the history, and
    // no pre-turn snapshot has been taken for it.
    let model =
        match crate::agent::execution::resolve_model(&config, &session_handle, &cancel_token).await
        {
            Ok(Some(model)) => model,
            Ok(None) => {
                return Ok(PromptRun::finished(PromptResponse::new(
                    StopReason::Cancelled,
                )));
            }
            Err(error) => {
                let message = error.to_string();
                config.emit_event(
                    &session_id,
                    AgentEventKind::Error {
                        message: message.clone(),
                    },
                );
                return Err(provider_failure_from_execution_error(
                    &error,
                    session_handle.llm_config(),
                )
                .unwrap_or(AgentError::Internal(message)));
            }
        };
    let env = make_env(model);

    debug!(
        "Session {}: user_message_stored, starting pre-turn snapshot",
        session_id
    );

    // Pre-turn snapshot for undo/redo (off critical path)
    if !restricted
        && let Some(ref backend) = config.snapshot_backend
        && let Some(worktree) = runtime.cwd.as_ref()
    {
        // The start and change records of a run share the run's turn id.
        let pre_turn_id = turn_id;
        let backend = Arc::clone(backend);
        let worktree = worktree.to_path_buf();
        let worktree_display = worktree.display().to_string();
        let session_id_for_task = session_id.clone();

        let task = tokio::spawn(
            async move {
                let started = std::time::Instant::now();
                let snapshot_result = backend.track(&worktree).await;
                let elapsed = started.elapsed();
                match &snapshot_result {
                    Ok(snapshot_id) => debug!(
                        "Session {}: pre-turn snapshot ready in {:?} (turn_id={}, snapshot_id={})",
                        session_id_for_task, elapsed, pre_turn_id, snapshot_id
                    ),
                    Err(err) => warn!(
                        "Session {}: pre-turn snapshot failed in {:?} (turn_id={}): {}",
                        session_id_for_task, elapsed, pre_turn_id, err
                    ),
                }
                (pre_turn_id, snapshot_result)
            }
            .instrument(info_span!(
                "agent.snapshot.pre_turn.track",
                session_id = %session_id,
                turn_id = %pre_turn_id,
                worktree = %worktree_display
            )),
        );

        turn.snapshot = PreTurnSnapshot::Pending(task);
    } else {
        debug!(
            "Session {}: no snapshot backend, skipping pre-turn snapshot",
            session_id
        );
        turn.snapshot = PreTurnSnapshot::None;
    }

    tracing::Span::current().record(
        "admission_ms",
        admission_started.elapsed().as_millis() as u64,
    );
    debug!(
        "Session {}: entering run_turn, cancelled={}",
        session_id,
        env.cancellation_token.is_cancelled()
    );

    // Execute Agent Loop using State Machine
    //
    // A panic of the loop is held until the post-turn snapshot is recorded: the
    // tools of the turn may have changed files, and undo needs those changes.
    // It is then raised again, for `execute_prompt_guarded` to report.
    let caught = {
        use futures_util::FutureExt as _;
        std::panic::AssertUnwindSafe(crate::agent::execution::run_turn(
            &config, &env, &mut turn, mode,
        ))
        .catch_unwind()
        .await
    };

    debug!(
        "Session {}: state machine returned: {}",
        session_id,
        match &caught {
            Ok(Ok(outcome)) => format!("{outcome:?}"),
            Ok(Err(_)) => "error".to_string(),
            Err(_) => "panic".to_string(),
        }
    );

    debug!("Session {}: post-turn snapshot start", session_id);

    // Post-turn snapshot
    if !restricted
        && let Some(ref backend) = config.snapshot_backend
        && let Some(worktree) = runtime.cwd.as_ref()
    {
        if let Err(e) = ensure_pre_turn_snapshot_ready(&env, &mut turn, "post_turn_snapshot").await
        {
            warn!(
                "Failed to resolve pre-turn snapshot before post-turn processing: {}",
                e
            );
        }

        let turn_snapshot_data = match std::mem::take(&mut turn.snapshot) {
            PreTurnSnapshot::Ready(snapshot) => Some(snapshot),
            PreTurnSnapshot::None | PreTurnSnapshot::Pending(_) => None,
        };
        if let Some(TurnSnapshot {
            turn_id,
            snapshot_id: pre_snapshot_id,
        }) = turn_snapshot_data
        {
            match backend
                .track(worktree)
                .instrument(info_span!("agent.snapshot.post_turn.track", session_id = %session_id))
                .await
            {
                Ok(post_snapshot_id) => {
                    if pre_snapshot_id != post_snapshot_id {
                        match backend
                            .diff(worktree, &pre_snapshot_id, &post_snapshot_id)
                            .instrument(info_span!(
                                "agent.snapshot.post_turn.diff",
                                session_id = %session_id
                            ))
                            .await
                        {
                            Ok(changed) if !changed.is_empty() => {
                                let patch_part = MessagePart::TurnSnapshotPatch {
                                    turn_id,
                                    snapshot_id: post_snapshot_id,
                                    base_snapshot_id: Some(pre_snapshot_id),
                                    workspace_root: Some(worktree.to_path_buf()),
                                    inherited: false,
                                    changed_paths: changed
                                        .iter()
                                        .map(|p| p.to_string_lossy().to_string())
                                        .collect(),
                                };
                                let snapshot_msg = AgentMessage {
                                    id: Uuid::new_v4().to_string(),
                                    session_id: session_id.clone(),
                                    role: ChatRole::Assistant,
                                    parts: vec![patch_part],
                                    created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                                    parent_message_id: None,
                                    source_provider: None,
                                    source_model: None,
                                };
                                if let Err(e) = env.store_snapshot_record(snapshot_msg).await {
                                    warn!("Failed to store turn snapshot patch: {}", e);
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!("Failed to diff turn snapshots: {}", e),
                        }
                    }
                }
                Err(e) => warn!("Post-turn snapshot failed: {}", e),
            }
            if config.snapshot_gc_config.is_enabled()
                && let Err(error) = backend
                    .gc(worktree, &config.snapshot_gc_config)
                    .instrument(info_span!(
                        "agent.snapshot.post_turn.gc",
                        session_id = %session_id
                    ))
                    .await
            {
                tracing::warn!(
                    %session_id,
                    %error,
                    "post-turn snapshot garbage collection failed"
                );
            }
        }
    } else {
        debug!(
            "Session {}: no snapshot backend, skipping post-turn snapshot",
            session_id
        );
    }

    debug!("Session {}: post-turn snapshot done", session_id);

    let result = caught.unwrap_or_else(|payload| std::panic::resume_unwind(payload));
    match result {
        Ok(CycleOutcome::Completed) => Ok(PromptRun::finished(PromptResponse::new(
            StopReason::EndTurn,
        ))),
        Ok(CycleOutcome::Cancelled) => Ok(PromptRun::finished(PromptResponse::new(
            StopReason::Cancelled,
        ))),
        Ok(CycleOutcome::Stopped {
            stop_reason,
            stop_type,
            reason,
        }) => Ok(PromptRun {
            response: PromptResponse::new(stop_reason),
            stop: Some((stop_type, reason)),
        }),
        Err(e) => {
            let message = e.to_string();
            config.emit_event(
                &session_id,
                AgentEventKind::Error {
                    message: message.clone(),
                },
            );
            Err(provider_failure_from_execution_error(&e, env.llm_config())
                .unwrap_or(AgentError::Internal(message)))
        }
    }
}

fn provider_failure_from_execution_error(
    error: &TurnError,
    llm_config: Option<&LLMConfig>,
) -> Option<AgentError> {
    let llm_error = error.llm_error()?;

    Some(AgentError::ProviderFailure {
        message: error.to_string(),
        provider: llm_config.map(|config| config.provider.clone()),
        model: llm_config.map(|config| config.model.clone()),
        retryable: llm_error.is_retryable(),
        error: Box::new(llm_error.to_payload()),
    })
}

#[instrument(
    name = "agent.snapshot.pre_turn.ensure",
    skip(env, turn),
    fields(session_id = %env.session_id, reason = reason)
)]
pub(crate) async fn ensure_pre_turn_snapshot_ready(
    env: &TurnEnv,
    turn: &mut Turn,
    reason: &'static str,
) -> Result<(), AgentError> {
    let task = match std::mem::take(&mut turn.snapshot) {
        PreTurnSnapshot::Pending(task) => task,
        // Nothing pending: keep a snapshot that is already ready.
        settled @ (PreTurnSnapshot::None | PreTurnSnapshot::Ready(_)) => {
            turn.snapshot = settled;
            return Ok(());
        }
    };

    let span = info_span!(
        "agent.snapshot.pre_turn.resolve",
        session_id = %env.session_id,
        reason = reason
    );

    match task.instrument(span).await {
        Ok((turn_id, Ok(snapshot_id))) => {
            // The runtime and the stored start record both own the id; a
            // `SnapshotId` clone is a reference-count increment.
            turn.snapshot = PreTurnSnapshot::Ready(TurnSnapshot {
                turn_id,
                snapshot_id: snapshot_id.clone(),
            });

            let start_part = MessagePart::TurnSnapshotStart {
                turn_id,
                snapshot_id,
                workspace_root: env.runtime.cwd.clone(),
                inherited: false,
            };
            let snapshot_msg = AgentMessage {
                id: Uuid::new_v4().to_string(),
                session_id: env.session_id.clone(),
                role: ChatRole::Assistant,
                parts: vec![start_part],
                created_at: time::OffsetDateTime::now_utc().unix_timestamp(),
                parent_message_id: None,
                source_provider: None,
                source_model: None,
            };
            env.store_snapshot_record(snapshot_msg).await.map_err(|e| {
                AgentError::Internal(format!("Failed to store turn snapshot start: {}", e))
            })?;
        }
        Ok((_turn_id, Err(err))) => {
            warn!("Pre-turn snapshot task finished with error: {}", err);
        }
        Err(join_err) => {
            warn!("Pre-turn snapshot task join failed: {}", join_err);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use kameo::actor::Spawn;
    use querymt::LLMParams;
    use querymt::error::{LLMErrorPayload, ProviderErrorKind, ProviderFailure};
    #[cfg(feature = "remote")]
    use querymt_remote::{
        NodeId,
        peer_access::{
            GrantStore, GrantStoreError, PeerGrant, PeerGrantKey, PeerGrantState, PolicyChange,
            PolicyEffect,
        },
    };
    use tokio::sync::Mutex;
    #[cfg(feature = "remote")]
    use tokio::sync::{Semaphore, broadcast};
    use tokio_util::sync::CancellationToken;

    use super::*;

    #[test]
    fn editor_bridge_is_disabled_for_workspace_bound_sessions() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let bridge = Some(ClientBridgeSender::new(tx));

        // Local sessions keep the editor bridge.
        assert!(
            editor_bridge_for_workspace(false, bridge.clone()).is_some(),
            "local sessions keep the editor bridge"
        );

        // Workspace-bound sessions must not silently serve the editor's
        // original checkout through the fixed-root bridge.
        assert!(
            editor_bridge_for_workspace(true, bridge).is_none(),
            "workspace-bound sessions must not serve the original checkout"
        );
        assert!(
            editor_bridge_for_workspace(true, None).is_none(),
            "no bridge stays no bridge"
        );
    }
    use crate::agent::agent_config_builder::AgentConfigBuilder;
    use crate::agent::core::ToolPolicy;
    use crate::session::backend::StorageBackend;
    use crate::session::domain::IntentSnapshot;
    use crate::session::error::{SessionError, SessionResult};
    use crate::session::projection::{EventJournal, NewDurableEvent};
    use crate::session::runtime::RuntimeContext;
    use crate::session::store::SessionStore;
    use crate::test_utils::{
        MockLlmProvider, MockSessionStore, SharedLlmProvider, TestProviderFactory, mock_llm_config,
        mock_plugin_registry, mock_session,
    };
    use querymt_agent_core::events::DurableEvent;

    #[tokio::test]
    async fn genai_detached_agent_uses_each_request_parent_and_closes_on_early_exits() {
        use opentelemetry::{
            Value,
            trace::{SpanId, SpanKind, Status, TraceId},
        };
        use querymt_agent_services::tools::trace_support::genai_trace::{
            assert_private, attr, capture,
        };

        let fixture = crate::test_utils::TestAgent::new().await;
        for (trace, cancelled, dropped, id) in [
            (
                Some("11111111111111111111111111111111"),
                false,
                false,
                Some("parent"),
            ),
            (
                Some("22222222222222222222222222222222"),
                true,
                false,
                Some("parent"),
            ),
            (None, false, true, None),
            (None, false, false, Some(" \t")),
        ] {
            let config = AgentConfigBuilder::from_provider(
                fixture.storage.clone(),
                Arc::new(
                    (*fixture.config.provider)
                        .clone()
                        .with_agent_id(id.map(str::to_owned)),
                ),
                fixture.storage.event_journal(),
            )
            .build();
            let runtime = SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            );
            let _permit = runtime.execution_permit.acquire().await.unwrap();
            let actor =
                SessionActor::new(Arc::new(config), "trace-session".into(), runtime.clone());
            let prompt = if cancelled || dropped {
                vec![crate::acp::protocol::ContentBlock::from("SECRET_PROMPT")]
            } else {
                vec![]
            };
            let mut req = crate::acp::protocol::PromptRequest::new("trace-session", prompt);
            if let Some(trace) = trace {
                req.meta = Some(serde_json::Map::from_iter([(
                    "traceparent".into(),
                    serde_json::Value::from(format!("00-{trace}-1234567890abcdef-01")),
                )]));
            }
            let trace_turn_id = querymt_agent_core::model::TurnId::new();
            let exec = DetachedPromptExecution {
                req,
                session_id: actor.session_id.clone(),
                runtime: actor.runtime.clone(),
                config: actor.config.clone(),
                cancel_token: CancellationToken::new(),
                mode: actor.mode,
                tool_config: actor.tool_config.clone(),
                execution_origin: ExecutionOrigin::Interactive,
                turn_id: trace_turn_id,
                steering: Arc::new(crate::agent::turn_control::SteeringInbox::new(
                    trace_turn_id,
                )),
                phase_reporter: tokio::sync::watch::channel(RunSignal::starting()).0,
            };
            if cancelled {
                exec.cancel_token.cancel();
            }
            let (_, spans) = capture(
                async {
                    if dropped {
                        let mut future = Box::pin(execute_prompt_detached(exec));
                        assert!(futures_util::poll!(&mut future).is_pending());
                        drop(future);
                    } else {
                        let result = execute_prompt_detached(exec).await.map(|run| run.response);
                        assert_eq!(result.is_ok(), cancelled);
                    }
                }
                .instrument(info_span!("unrelated-actor")),
            )
            .await;
            let agent = spans.iter().find(|s| s.name == "invoke_agent").unwrap();
            assert_eq!(agent.span_kind, SpanKind::Internal);
            assert_eq!(
                attr(agent, "gen_ai.operation.name"),
                Some(&Value::from("invoke_agent"))
            );
            assert_eq!(
                attr(agent, "gen_ai.conversation.id"),
                Some(&Value::from("trace-session"))
            );
            assert_eq!(
                attr(agent, "session.id"),
                attr(agent, "gen_ai.conversation.id")
            );
            assert!(attr(agent, "gen_ai.request.model").is_none());
            assert!(attr(agent, "gen_ai.agent.name").is_none());
            assert_eq!(
                attr(agent, "gen_ai.agent.id"),
                id.filter(|id| !id.trim().is_empty())
                    .map(Value::from)
                    .as_ref()
            );
            if let Some(trace) = trace {
                assert_eq!(
                    agent.span_context.trace_id(),
                    TraceId::from_hex(trace).unwrap()
                );
                assert_eq!(
                    agent.parent_span_id,
                    SpanId::from_hex("1234567890abcdef").unwrap()
                );
            } else {
                assert_eq!(agent.parent_span_id, SpanId::INVALID);
            }
            assert_eq!(
                agent.status,
                if cancelled || dropped {
                    Status::Unset
                } else {
                    Status::error("")
                }
            );
            let error_type = Value::from("invalid_prompt");
            assert_eq!(
                attr(agent, "error.type"),
                if cancelled || dropped {
                    None
                } else {
                    Some(&error_type)
                }
            );
            assert_private(&spans);
        }
    }

    // ── Shared fixture ───────────────────────────────────────────────────────

    struct FailNextAppendJournal {
        inner: Arc<dyn EventJournal>,
        fail_next_append: AtomicBool,
    }

    impl FailNextAppendJournal {
        fn new(inner: Arc<dyn EventJournal>) -> Self {
            Self {
                inner,
                fail_next_append: AtomicBool::new(false),
            }
        }

        fn fail_next_append(&self) {
            self.fail_next_append.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl EventJournal for FailNextAppendJournal {
        async fn append_durable(&self, event: &NewDurableEvent) -> SessionResult<DurableEvent> {
            if self.fail_next_append.swap(false, Ordering::SeqCst) {
                return Err(SessionError::Other(
                    "simulated event append failure".to_string(),
                ));
            }
            self.inner.append_durable(event).await
        }

        async fn load_session_stream(
            &self,
            session_id: &str,
            after_seq: Option<i64>,
            limit: Option<usize>,
        ) -> SessionResult<Vec<DurableEvent>> {
            self.inner
                .load_session_stream(session_id, after_seq, limit)
                .await
        }

        async fn load_global_stream(
            &self,
            after_seq: Option<i64>,
            limit: Option<usize>,
        ) -> SessionResult<Vec<DurableEvent>> {
            self.inner.load_global_stream(after_seq, limit).await
        }

        async fn delete_session_events_range(
            &self,
            session_id: &str,
            from_seq: i64,
            before_seq: Option<i64>,
        ) -> SessionResult<usize> {
            self.inner
                .delete_session_events_range(session_id, from_seq, before_seq)
                .await
        }

        async fn append_durable_from_source(
            &self,
            event: &NewDurableEvent,
        ) -> SessionResult<Option<DurableEvent>> {
            self.inner.append_durable_from_source(event).await
        }

        async fn latest_source_seq(
            &self,
            session_id: &str,
            source_node_id: &str,
        ) -> SessionResult<Option<i64>> {
            self.inner
                .latest_source_seq(session_id, source_node_id)
                .await
        }

        async fn remote_sync_cursor(
            &self,
            session_id: &str,
            source_node_id: &str,
        ) -> SessionResult<Option<i64>> {
            self.inner
                .remote_sync_cursor(session_id, source_node_id)
                .await
        }

        async fn advance_remote_sync_cursor(
            &self,
            session_id: &str,
            source_node_id: &str,
            source_seq: i64,
            complete: bool,
        ) -> SessionResult<()> {
            self.inner
                .advance_remote_sync_cursor(session_id, source_node_id, source_seq, complete)
                .await
        }

        async fn load_remote_session_stream(
            &self,
            session_id: &str,
            source_node_id: &str,
        ) -> SessionResult<Vec<DurableEvent>> {
            self.inner
                .load_remote_session_stream(session_id, source_node_id)
                .await
        }

        async fn max_stream_seq(&self, session_id: &str) -> SessionResult<i64> {
            self.inner.max_stream_seq(session_id).await
        }

        async fn history_epoch(&self, session_id: &str) -> SessionResult<u64> {
            self.inner.history_epoch(session_id).await
        }
    }

    struct ActorFixture {
        config: Arc<AgentConfig>,
        actor_ref: kameo::actor::ActorRef<SessionActor>,
        execution_permit: Arc<tokio::sync::Semaphore>,
        _session_id: String,
        _temp_dir: tempfile::TempDir,
        event_journal: Arc<FailNextAppendJournal>,
        intent_writes: Arc<AtomicUsize>,
        history_writes: Arc<AtomicUsize>,
        lease_dir: tempfile::TempDir,
        session_deleted: Arc<AtomicBool>,
    }

    impl ActorFixture {
        fn lease_root(&self) -> RunLeaseRoot {
            RunLeaseRoot::new(self.lease_dir.path().join("leases"))
        }

        /// `(event type, run_id)` of the durable run lifecycle events.
        async fn run_lifecycle(&self) -> Vec<(&'static str, String, Option<String>)> {
            self.config
                .storage
                .event_journal()
                .load_session_stream(&self._session_id, None, None)
                .await
                .expect("load journal")
                .into_iter()
                .filter_map(|event| match event.kind {
                    AgentEventKind::RunStarted { run_id, .. } => {
                        Some(("run_started", run_id, None))
                    }
                    AgentEventKind::RunCompleted {
                        run_id, outcome, ..
                    } => Some(("run_completed", run_id, Some(outcome))),
                    _ => None,
                })
                .collect()
        }

        async fn wait_for_completed_runs(&self, count: usize) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let completed = self
                    .run_lifecycle()
                    .await
                    .iter()
                    .filter(|(kind, _, _)| *kind == "run_completed")
                    .count();
                if completed >= count {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for {count} completed run(s)"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }

        async fn new() -> Self {
            Self::with_session_id("test-session").await
        }

        async fn turn(&self, session_id: &str) -> (TurnEnv, Turn) {
            let session_handle = self
                .config
                .provider
                .with_session(session_id)
                .await
                .expect("load session handle");
            let runtime_context =
                RuntimeContext::new(self.config.provider.history_store(), session_id.to_string())
                    .await
                    .expect("create runtime context");
            let runtime = crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            );
            crate::test_utils::turn_parts(&self.config, session_handle, runtime, runtime_context)
                .await
        }

        async fn with_session_id(session_id: &str) -> Self {
            let mut llm = MockLlmProvider::new();
            llm.expect_chat().returning(|_| {
                Ok(crate::test_utils::MockChatResponse::text_only("actor fixture reply").into())
            });
            let provider = Arc::new(Mutex::new(llm));
            let shared = SharedLlmProvider {
                inner: provider.clone(),
                tools: vec![].into_boxed_slice(),
            };
            let factory = Arc::new(TestProviderFactory::new(shared));
            let (plugin_registry, temp_dir) =
                mock_plugin_registry(factory).expect("plugin registry");

            let llm_config = mock_llm_config();
            let session = mock_session(session_id);
            let mut store = MockSessionStore::new();
            store
                .expect_peek_revert_state()
                .returning(|_| Ok(None))
                .times(0..);
            store
                .expect_get_history()
                .returning(|_| Ok(Vec::new()))
                .times(0..);
            store
                .expect_get_effective_history()
                .returning(|_| Ok(Vec::new()))
                .times(0..);
            store
                .expect_append_progress_entry()
                .returning(|_| Ok(()))
                .times(0..);
            let intent_writes = Arc::new(AtomicUsize::new(0));
            let history_writes = Arc::new(AtomicUsize::new(0));
            let current_intent = Arc::new(std::sync::Mutex::new(None));
            let session_clone = session.clone();
            let session_deleted = Arc::new(AtomicBool::new(false));
            let deleted_for_get = session_deleted.clone();
            store
                .expect_get_session()
                .returning(move |_| {
                    if deleted_for_get.load(Ordering::SeqCst) {
                        Ok(None)
                    } else {
                        Ok(Some(session_clone.clone()))
                    }
                })
                .times(0..);
            let llm_for_mock = llm_config.clone();
            store
                .expect_get_session_llm_config()
                .returning(move |_| Ok(Some(llm_for_mock.clone())))
                .times(0..);
            store
                .expect_get_session_control()
                .returning(|_| Ok(None))
                .times(0..);
            store
                .expect_commit_session_control()
                .returning(|_, _, state| Ok(state.clone()))
                .times(0..);
            let llm_config_for_mock = llm_config.clone();
            store
                .expect_get_llm_config()
                .returning(move |_| Ok(Some(llm_config_for_mock.clone())))
                .times(0..);
            store
                .expect_get_session_execution_config()
                .returning(|_| Ok(None))
                .times(0..);
            store
                .expect_create_or_get_llm_config()
                .returning(|_| Ok(mock_llm_config()))
                .times(0..);
            store
                .expect_set_session_llm_config()
                .returning(|_, _| Ok(()))
                .times(0..);
            store
                .expect_set_session_provider_node_id()
                .returning(|_, _| Ok(()))
                .times(0..);
            store
                .expect_get_session_provider_node_id()
                .returning(|_| Ok(None))
                .times(0..);
            store
                .expect_list_sessions()
                .returning(|| Ok(vec![]))
                .times(0..);
            // The one task a scheduled run asks for (`scheduled_prompt`) is active.
            let task_session_id = session.id;
            store
                .expect_get_task()
                .returning(move |task_public_id| {
                    if task_public_id != "task-1" {
                        return Ok(None);
                    }
                    let now = time::OffsetDateTime::now_utc();
                    Ok(Some(crate::session::domain::Task {
                        id: 1,
                        public_id: task_public_id.to_string(),
                        session_id: task_session_id,
                        kind: crate::session::domain::TaskKind::Recurring,
                        status: crate::session::domain::TaskStatus::Active,
                        expected_deliverable: Some("scheduled work".to_string()),
                        acceptance_criteria: None,
                        revision: 1,
                        creation_key: None,
                        completion_evidence: None,
                        completed_at: None,
                        created_at: now,
                        updated_at: now,
                    }))
                })
                .times(0..);
            store
                .expect_get_remote_session_bookmark()
                .returning(|_| Ok(None))
                .times(0..);
            store
                .expect_get_session_runtime_binding()
                .returning(|_| Ok(None))
                .times(0..);
            let writes_for_history = history_writes.clone();
            store
                .expect_add_message()
                .returning(move |_, _| {
                    writes_for_history.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .times(0..);
            let writes_for_create = intent_writes.clone();
            let intent_for_create = current_intent.clone();
            store
                .expect_create_and_set_current_intent_snapshot()
                .returning(move |_, mut snapshot| {
                    writes_for_create.fetch_add(1, Ordering::SeqCst);
                    snapshot.id = 1;
                    *intent_for_create.lock().expect("intent lock") = Some(snapshot.clone());
                    Ok(snapshot)
                })
                .times(0..);
            let intent_for_get = current_intent.clone();
            store
                .expect_get_current_intent_snapshot()
                .returning(move |_| Ok(intent_for_get.lock().expect("intent lock").clone()))
                .times(0..);

            let lease_dir = tempfile::TempDir::new().expect("lease dir");
            let mock_storage = Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create event store")
                    .with_run_lease_root(lease_dir.path().join("leases")),
            );
            let store: Arc<dyn SessionStore> = Arc::new(store);
            let provider = Arc::new(crate::session::provider::SessionProvider::new(
                Arc::new(plugin_registry),
                store,
                LLMParams::new().provider("mock").model("mock-model"),
            ));
            let event_journal = Arc::new(FailNextAppendJournal::new(mock_storage.event_journal()));
            let config = Arc::new(
                AgentConfigBuilder::from_provider(
                    mock_storage.clone(),
                    provider,
                    event_journal.clone(),
                )
                .with_tool_policy(ToolPolicy::ProviderOnly)
                .build(),
            );

            let runtime = crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            );
            let execution_permit = runtime.execution_permit.clone();
            let actor = SessionActor::new(config.clone(), session_id.to_string(), runtime);
            let actor_ref = SessionActor::spawn(actor);

            Self {
                config,
                actor_ref,
                execution_permit,
                _session_id: session_id.to_string(),
                _temp_dir: temp_dir,
                event_journal,
                intent_writes,
                history_writes,
                lease_dir,
                session_deleted,
            }
        }
    }

    fn text_submit(session_id: &str, text: &str) -> SubmitSessionInput {
        SubmitSessionInput::new(SubmitInput {
            session_id: session_id.to_string(),
            client_input_id: None,
            expected_run_id: None,
            delivery: InputDelivery::Queue,
            prompt: vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new(text),
            )],
        })
    }

    async fn seed_orphan(f: &ActorFixture, run_id: &str) {
        f.config
            .storage
            .event_journal()
            .append_durable(&crate::session::projection::NewDurableEvent {
                session_seq: None,
                session_id: f._session_id.clone(),
                origin: querymt_agent_core::events::EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                kind: AgentEventKind::RunStarted {
                    run_id: run_id.to_string(),
                    origin: "interactive".to_string(),
                },
            })
            .await
            .expect("seed orphan");
    }

    #[tokio::test]
    async fn reconcile_on_cancel_of_idle_actor_closes_orphaned_run_once() {
        let f = ActorFixture::new().await;
        seed_orphan(&f, "orphan").await;

        f.actor_ref.tell(Cancel).await.unwrap();
        f.wait_for_completed_runs(1).await;
        // A second cancel is a no-op for the already-closed run.
        f.actor_ref.tell(Cancel).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let completed: Vec<_> = f
            .run_lifecycle()
            .await
            .into_iter()
            .filter(|(kind, _, _)| *kind == "run_completed")
            .collect();
        assert_eq!(
            completed,
            vec![(
                "run_completed",
                "orphan".to_string(),
                Some("interrupted".to_string())
            )]
        );
    }

    #[tokio::test]
    async fn cancel_during_live_run_does_not_reconcile() {
        let f = ActorFixture::new().await;
        let SubmitInputResult::Started { run_id, .. } = f
            .actor_ref
            .ask(text_submit(&f._session_id, "hi"))
            .await
            .unwrap()
        else {
            panic!("expected Started");
        };

        f.actor_ref.tell(Cancel).await.unwrap();
        f.actor_ref.ask(GetRuntimeStatus).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // The live run holds the lease, so even a racing reconcile finds it
        // busy; nothing may be journaled as interrupted for it.
        assert!(
            f.run_lifecycle()
                .await
                .iter()
                .all(|(_, id, outcome)| id != &run_id || outcome.as_deref() != Some("interrupted"))
        );
    }

    #[tokio::test]
    async fn busy_run_lease_rejects_prompt_without_run_started() {
        let f = ActorFixture::new().await;
        let _held = f
            .lease_root()
            .try_acquire(&f._session_id)
            .unwrap()
            .expect("lease free");

        let result = f.actor_ref.ask(text_submit(&f._session_id, "hi")).await;
        assert!(matches!(
            result,
            Err(kameo::error::SendError::HandlerError(
                AgentError::SessionRunningElsewhere { .. }
            ))
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(f.run_lifecycle().await.is_empty());
        let status = f.actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(status.phase, SessionRuntimePhase::Idle);
    }

    fn scheduled_prompt() -> ScheduledPrompt {
        ScheduledPrompt {
            schedule_public_id: "schedule-1".to_string(),
            turn_id: querymt_agent_core::model::TurnId::new(),
            task_public_id: "task-1".to_string(),
            prompt_text: "scheduled work".to_string(),
            execution_limits: None,
            origin: crate::session::domain_schedule::ScheduleOrigin::Local,
        }
    }

    /// Durable events of the session, as `(event name, run id if it has one)`,
    /// in journal order, with the `origin` of each `run_started`.
    async fn journal_overview(f: &ActorFixture) -> Vec<(String, Option<String>, Option<String>)> {
        f.config
            .event_sink
            .session_stream(&f._session_id)
            .flush()
            .await;
        f.config
            .storage
            .event_journal()
            .load_session_stream(&f._session_id, None, None)
            .await
            .expect("load journal")
            .into_iter()
            .map(|event| {
                let name = event.kind.tag().to_string();
                match event.kind {
                    AgentEventKind::RunStarted { run_id, origin } => {
                        (name, Some(run_id), Some(origin))
                    }
                    AgentEventKind::RunCompleted { run_id, .. } => (name, Some(run_id), None),
                    AgentEventKind::QueuedInputStarted { run_id, .. } => (name, Some(run_id), None),
                    _ => (name, None, None),
                }
            })
            .collect()
    }

    fn run_starts(overview: &[(String, Option<String>, Option<String>)]) -> Vec<(String, String)> {
        overview
            .iter()
            .filter(|(name, ..)| name == "run_started")
            .map(|(_, run_id, origin)| (run_id.clone().unwrap(), origin.clone().unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn scheduled_run_has_a_start_and_a_completion() {
        let f = ActorFixture::new().await;

        f.actor_ref
            .ask(scheduled_prompt())
            .await
            .expect("scheduled run completes");
        f.wait_for_completed_runs(1).await;

        let overview = journal_overview(&f).await;
        let starts = run_starts(&overview);
        assert_eq!(starts.len(), 1, "one run start: {overview:?}");
        assert_eq!(starts[0].1, "scheduled");
        let completions: Vec<_> = overview
            .iter()
            .filter(|(name, ..)| name == "run_completed")
            .collect();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].1.as_ref(), Some(&starts[0].0));
        let started_at = overview
            .iter()
            .position(|(name, ..)| name == "run_started")
            .unwrap();
        assert_eq!(
            started_at, 0,
            "the start precedes every other event: {overview:?}"
        );
    }

    #[tokio::test]
    async fn interactive_run_start_is_unchanged() {
        let f = ActorFixture::new().await;

        let SubmitInputResult::Started { run_id, .. } = f
            .actor_ref
            .ask(text_submit(&f._session_id, "hi"))
            .await
            .unwrap()
        else {
            panic!("expected Started");
        };
        f.wait_for_completed_runs(1).await;

        let starts = run_starts(&journal_overview(&f).await);
        assert_eq!(starts, vec![(run_id, "interactive".to_string())]);
    }

    #[tokio::test]
    async fn queued_prompt_run_start_is_unchanged() {
        let f = ActorFixture::new().await;
        // Hold the first run open so the second input is queued behind it.
        let permit = f.execution_permit.acquire().await.unwrap();
        let SubmitInputResult::Started { run_id: first, .. } = f
            .actor_ref
            .ask(text_submit(&f._session_id, "first"))
            .await
            .unwrap()
        else {
            panic!("expected Started");
        };
        let SubmitInputResult::Queued { .. } = f
            .actor_ref
            .ask(text_submit(&f._session_id, "second"))
            .await
            .unwrap()
        else {
            panic!("expected Queued");
        };
        drop(permit);
        f.wait_for_completed_runs(2).await;

        let overview = journal_overview(&f).await;
        let starts = run_starts(&overview);
        assert_eq!(starts.len(), 2, "{overview:?}");
        assert_eq!(starts[0], (first, "interactive".to_string()));
        assert_eq!(starts[1].1, "interactive");
        let second_start = overview
            .iter()
            .position(|(name, run_id, _)| {
                name == "run_started" && run_id.as_ref() == Some(&starts[1].0)
            })
            .unwrap();
        assert_eq!(
            overview[second_start + 1],
            (
                "queued_input_started".to_string(),
                Some(starts[1].0.clone()),
                None
            ),
            "the queued-input event follows its run start"
        );
    }

    #[tokio::test]
    async fn scheduled_execution_rejected_while_a_run_is_active() {
        let f = ActorFixture::new().await;
        let execution = f.execution_permit.acquire().await.unwrap();
        let SubmitInputResult::Started { run_id, .. } = f
            .actor_ref
            .ask(text_submit(&f._session_id, "hi"))
            .await
            .unwrap()
        else {
            panic!("expected Started");
        };

        let result = f.actor_ref.ask(scheduled_prompt()).await;

        assert!(matches!(
            result,
            Err(kameo::error::SendError::HandlerError(
                AgentError::AdmissionRejected { .. }
            ))
        ));
        let starts = run_starts(&journal_overview(&f).await);
        assert_eq!(starts, vec![(run_id, "interactive".to_string())]);

        // Let the held run finish so no task outlives the test.
        drop(execution);
        f.wait_for_completed_runs(1).await;
    }

    #[tokio::test]
    async fn scheduled_execution_rejected_by_the_run_lease() {
        let f = ActorFixture::new().await;
        let _held = f
            .lease_root()
            .try_acquire(&f._session_id)
            .unwrap()
            .expect("lease free");

        let result = f.actor_ref.ask(scheduled_prompt()).await;

        assert!(matches!(
            result,
            Err(kameo::error::SendError::HandlerError(
                AgentError::SessionRunningElsewhere { .. }
            ))
        ));
        let overview = journal_overview(&f).await;
        assert!(run_starts(&overview).is_empty(), "{overview:?}");
        assert!(
            overview
                .iter()
                .any(|(name, ..)| name == "scheduled_execution_failed"),
            "{overview:?}"
        );
    }

    #[tokio::test]
    async fn crashed_scheduled_run_is_reconciled() {
        let f = ActorFixture::new().await;
        f.config
            .storage
            .event_journal()
            .append_durable(&crate::session::projection::NewDurableEvent {
                session_seq: None,
                session_id: f._session_id.clone(),
                origin: querymt_agent_core::events::EventOrigin::Local,
                source_node: None,
                source_node_id: None,
                source_seq: None,
                kind: AgentEventKind::RunStarted {
                    run_id: "crashed".to_string(),
                    origin: "scheduled".to_string(),
                },
            })
            .await
            .expect("seed crashed scheduled run");

        f.actor_ref.tell(Cancel).await.unwrap();
        f.wait_for_completed_runs(1).await;

        let completed: Vec<_> = f
            .run_lifecycle()
            .await
            .into_iter()
            .filter(|(kind, _, _)| *kind == "run_completed")
            .collect();
        assert_eq!(
            completed,
            vec![(
                "run_completed",
                "crashed".to_string(),
                Some("interrupted".to_string())
            )]
        );
    }

    #[tokio::test]
    async fn deleted_session_does_not_start_a_run_or_keep_the_lease() {
        let f = ActorFixture::new().await;
        f.session_deleted.store(true, Ordering::SeqCst);

        let result = f.actor_ref.ask(text_submit(&f._session_id, "hi")).await;
        assert!(matches!(
            result,
            Err(kameo::error::SendError::HandlerError(
                AgentError::SessionNotFound { .. }
            ))
        ));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(f.run_lifecycle().await.is_empty());
        assert!(
            f.lease_root()
                .try_acquire(&f._session_id)
                .unwrap()
                .is_some(),
            "lease must be released when the session no longer exists"
        );
    }

    #[tokio::test]
    async fn steering_a_builtin_is_rejected_by_the_actor() {
        let f = ActorFixture::new().await;
        let mut submit = text_submit(&f._session_id, "/compact");
        submit.input.delivery = InputDelivery::Steer;
        submit.input.expected_run_id = Some("run-1".to_string());

        let result = f.actor_ref.ask(submit).await;
        let expected = crate::slash_commands::builtin::BuiltinError::NotAccepted {
            builtin: Builtin::Compact,
            via: Submission::Steer,
        }
        .to_string();
        match result {
            Err(kameo::error::SendError::HandlerError(AgentError::InvalidPromptContent {
                message,
            })) => assert_eq!(message, expected),
            other => panic!("expected InvalidPromptContent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn queueing_worktree_is_rejected_before_a_receipt_is_issued() {
        let f = ActorFixture::new().await;

        let result = f
            .actor_ref
            .ask(text_submit(&f._session_id, "/worktree list"))
            .await;

        let expected = crate::slash_commands::builtin::BuiltinError::NotAccepted {
            builtin: Builtin::Worktree,
            via: Submission::Turn,
        }
        .to_string();
        match result {
            Err(kameo::error::SendError::HandlerError(AgentError::InvalidPromptContent {
                message,
            })) => assert_eq!(message, expected),
            other => panic!("expected InvalidPromptContent, got {other:?}"),
        }
        let status = f.actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(status.phase, SessionRuntimePhase::Idle);
    }

    #[tokio::test]
    async fn run_lease_momentary_holder_delays_but_does_not_reject_prompt() {
        let f = ActorFixture::new().await;
        // Simulates a reconciler or delete holding the lease for a moment.
        let held = f
            .lease_root()
            .try_acquire(&f._session_id)
            .unwrap()
            .expect("lease free");
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            drop(held);
        });

        let result = f
            .actor_ref
            .ask(text_submit(&f._session_id, "hi"))
            .await
            .expect("prompt must wait out a momentary holder");
        assert!(matches!(result, SubmitInputResult::Started { .. }));
        release.await.unwrap();
        assert!(
            f.lease_root()
                .try_acquire(&f._session_id)
                .unwrap()
                .is_none(),
            "started run must hold the lease"
        );
    }

    #[tokio::test]
    async fn run_lease_is_held_for_the_run_and_released_after_prompt_finished() {
        let f = ActorFixture::new().await;
        // This actor-only gate drives PromptFinished itself; inference must not race it.
        let _execution = f.execution_permit.acquire().await.unwrap();
        let started = f
            .actor_ref
            .ask(text_submit(&f._session_id, "hi"))
            .await
            .expect("submit");
        let SubmitInputResult::Started { run_id, .. } = started else {
            panic!("expected Started");
        };
        let root = f.lease_root();
        let observer = crate::agent::LocalAgentHandle::from_config(f.config.clone());
        observer
            .registry
            .lock()
            .await
            .insert(f._session_id.clone(), f.actor_ref.clone());
        let observed = observer
            .observe_session_runtimes(std::slice::from_ref(&f._session_id))
            .await
            .remove(&f._session_id)
            .unwrap();
        assert_eq!(observed.active_run_id.as_deref(), Some(run_id.as_str()));
        assert_ne!(
            observed.phase,
            SessionRuntimePhase::Unknown,
            "an active actor's own lease is not an external owner"
        );
        assert!(
            root.try_acquire(&f._session_id).unwrap().is_none(),
            "lease must be held while the run is active"
        );

        // Finish the run deterministically, independent of the mock provider.
        f.actor_ref
            .tell(PromptFinished {
                generation: 1,
                run_id: querymt_agent_core::model::TurnId::from_stored(&run_id),
                disposition: RunCompletionDisposition::Completed,
            })
            .await
            .unwrap();
        f.wait_for_completed_runs(1).await;
        let status = f.actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(status.phase, SessionRuntimePhase::Idle);
        assert!(
            root.try_acquire(&f._session_id).unwrap().is_some(),
            "lease must be released once the run is closed"
        );
        let events = f.run_lifecycle().await;
        assert_eq!(events[0], ("run_started", run_id.clone(), None));
        assert_eq!(
            events[1],
            ("run_completed", run_id, Some("completed".to_string()))
        );
        f.execution_permit.close();
    }

    #[tokio::test]
    async fn run_lease_is_handed_over_to_queued_prompt_without_release() {
        let f = ActorFixture::new().await;
        let _execution = f.execution_permit.acquire().await.unwrap();
        let mut runtime_events = f.config.subscribe_events();
        let SubmitInputResult::Started { run_id: first, .. } = f
            .actor_ref
            .ask(text_submit(&f._session_id, "one"))
            .await
            .unwrap()
        else {
            panic!("expected Started");
        };
        let queued = f
            .actor_ref
            .ask(text_submit(&f._session_id, "two"))
            .await
            .unwrap();
        assert!(matches!(queued, SubmitInputResult::Queued { .. }));

        f.actor_ref
            .tell(PromptFinished {
                generation: 1,
                run_id: querymt_agent_core::model::TurnId::from_stored(&first),
                disposition: RunCompletionDisposition::Completed,
            })
            .await
            .unwrap();
        f.wait_for_completed_runs(1).await;

        // The second run started with its own run id, under the lease handed
        // over from the first run (never released in between).
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = f.actor_ref.ask(GetRuntimeStatus).await.unwrap();
                if status
                    .active_run_id
                    .as_deref()
                    .is_some_and(|id| id != first)
                {
                    break status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("queued run B must start under the same lease");
        let second = status.active_run_id.clone().unwrap();
        let revision_b = status.observation.revision.unwrap();
        f.actor_ref
            .tell(PromptFinished {
                generation: 1,
                run_id: querymt_agent_core::model::TurnId::from_stored(&first),
                disposition: RunCompletionDisposition::Completed,
            })
            .await
            .unwrap();
        let after_stale = f.actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(after_stale.active_run_id.as_deref(), Some(second.as_str()));
        assert_eq!(after_stale.observation.revision, Some(revision_b));
        f.config
            .event_sink
            .session_stream(&f._session_id)
            .flush()
            .await;
        let mut seen_a = false;
        let mut seen_b = false;
        let mut previous_revision = 0;
        while let Ok(event) = runtime_events.try_recv() {
            if let AgentEventKind::RuntimeChanged { runtime } = event.kind() {
                assert_ne!(
                    runtime.phase,
                    SessionRuntimePhase::Idle,
                    "A-to-B handoff must not publish Idle"
                );
                assert!(runtime.observation.revision.unwrap() > previous_revision);
                previous_revision = runtime.observation.revision.unwrap();
                seen_a |= runtime.active_run_id.as_deref() == Some(first.as_str());
                seen_b |= runtime.active_run_id.as_deref() == Some(second.as_str());
            }
        }
        assert!(seen_a && seen_b);
        assert!(
            f.lease_root()
                .try_acquire(&f._session_id)
                .unwrap()
                .is_none(),
            "queued run must hold the lease"
        );

        f.actor_ref
            .tell(PromptFinished {
                generation: 2,
                run_id: querymt_agent_core::model::TurnId::from_stored(&second),
                disposition: RunCompletionDisposition::Completed,
            })
            .await
            .unwrap();
        f.wait_for_completed_runs(2).await;
        let events = f.run_lifecycle().await;
        for run_id in [&first, &second] {
            assert_eq!(
                events
                    .iter()
                    .filter(|(kind, id, _)| *kind == "run_started" && id == run_id)
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|(kind, id, _)| *kind == "run_completed" && id == run_id)
                    .count(),
                1
            );
        }
        f.execution_permit.close();
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn unsubscribe_disambiguates_relay_sequence_ids_by_peer_scoped_name() {
        use crate::agent::remote::{event_forwarder::EventForwarder, event_relay::EventRelayActor};
        crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let fixture = ActorFixture::new().await;
        let runtime = SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        let mut actor = SessionActor::new(fixture.config.clone(), "test-session".into(), runtime);
        let relay = EventRelayActor::spawn(EventRelayActor::new(
            fixture.config.event_sink.clone(),
            "test-session".into(),
            "viewer".into(),
            None,
            None,
        ));
        let relay_ref = relay.clone().into_remote_ref().await;
        let handle = || {
            EventForwarder::start(
                fixture.config.event_sink.fanout().clone(),
                relay_ref.clone(),
                "test".into(),
                "test-session".into(),
            )
        };
        let a = kameo::actor::ActorId::new_with_peer_id(
            42,
            libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id(),
        );
        let b = kameo::actor::ActorId::new_with_peer_id(
            42,
            libp2p::identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id(),
        );
        actor
            .relay_forwarder_handles
            .insert(a, (Some("relay/peer-a".into()), handle()));
        assert!(
            actor.remove_relay_forwarder(42, "relay/peer-b").is_none(),
            "another peer's sequence ID cannot unsubscribe A"
        );
        actor
            .relay_forwarder_handles
            .insert(b, (Some("relay/peer-b".into()), handle()));
        actor
            .remove_relay_forwarder(42, "relay/peer-b")
            .expect("matching B forwarder")
            .abort();
        assert!(actor.relay_forwarder_handles.contains_key(&a));
        assert!(!actor.relay_forwarder_handles.contains_key(&b));
        relay.kill();
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn event_subscriptions_require_a_name_even_with_direct_capability() {
        use crate::agent::remote::event_relay::EventRelayActor;
        crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let fixture = ActorFixture::new().await;
        let relay = EventRelayActor::spawn(EventRelayActor::new(
            fixture.config.event_sink.clone(),
            "test-session".into(),
            "viewer".into(),
            None,
            None,
        ));
        let relay_ref = relay.clone().into_remote_ref().await;
        for direct in [false, true] {
            for name in [None, Some(String::new()), Some("  ".into())] {
                let error = fixture
                    .actor_ref
                    .ask(crate::agent::messages::SubscribeEvents {
                        relay_actor_id: relay.id().sequence_id(),
                        relay_ref: direct.then(|| relay_ref.clone()),
                        relay_dht_name: name,
                        after_source_seq: None,
                    })
                    .await
                    .expect_err("unremovable subscription rejected");
                assert!(
                    error
                        .to_string()
                        .contains("requires a non-empty relay_dht_name")
                );
            }
        }
        fixture
            .actor_ref
            .ask(crate::agent::messages::SubscribeEvents {
                relay_actor_id: relay.id().sequence_id(),
                relay_ref: Some(relay_ref),
                relay_dht_name: Some("relay/viewer".into()),
                after_source_seq: None,
            })
            .await
            .expect("named direct subscription");
        fixture
            .actor_ref
            .ask(crate::agent::messages::UnsubscribeEvents {
                relay_actor_id: relay.id().sequence_id(),
                relay_dht_name: "relay/viewer".into(),
            })
            .await
            .expect("named unsubscribe");
        relay.kill();
        fixture.actor_ref.kill();
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn only_observer_link_failures_are_isolated() {
        use crate::agent::remote::event_relay::EventRelayActor;
        crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let fixture = ActorFixture::new().await;
        let relay = EventRelayActor::spawn(EventRelayActor::new(
            fixture.config.event_sink.clone(),
            "test-session".into(),
            "viewer".into(),
            None,
            None,
        ));
        fixture
            .actor_ref
            .ask(SubscribeEvents {
                relay_actor_id: relay.id().sequence_id(),
                relay_ref: Some(relay.clone().into_remote_ref().await),
                relay_dht_name: Some("relay/observer".into()),
                after_source_seq: None,
            })
            .await
            .unwrap();
        relay.link(&fixture.actor_ref).await;
        relay.kill();
        relay.wait_for_shutdown().await;
        // Drain the link notification before asking the owner to change its state.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        fixture
            .actor_ref
            .ask(SetReasoningEffort {
                effort: Some(ReasoningEffort::High),
            })
            .await
            .expect("observer death must not stop the session");
        assert_eq!(
            fixture.actor_ref.ask(GetReasoningEffort).await.unwrap(),
            Some(ReasoningEffort::High)
        );
        // Unrelated linked actors retain Kameo's failure propagation behavior.
        let unrelated = EventRelayActor::spawn(EventRelayActor::new(
            fixture.config.event_sink.clone(),
            "test-session".into(),
            "unrelated".into(),
            None,
            None,
        ));
        unrelated.link(&fixture.actor_ref).await;
        unrelated.kill();
        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            fixture.actor_ref.wait_for_shutdown_result(),
        )
        .await
        .expect("unrelated actor failures still propagate")
        .unwrap();
        assert!(matches!(
            stopped,
            kameo::error::ActorStopReason::LinkDied { .. }
        ));
    }

    // ── Cursor-based event stream paging (plan §16) ──────────────────────────

    #[tokio::test]
    async fn get_event_stream_since_returns_page_and_tip() {
        let fixture = ActorFixture::new().await;
        for _ in 0..3 {
            fixture
                .config
                .emit_event_persisted(
                    "test-session",
                    querymt_agent_core::events::AgentEventKind::SessionCreated,
                )
                .await
                .unwrap();
        }

        let session_ref = crate::agent::remote::SessionActorRef::from(fixture.actor_ref.clone());

        // Full page from the beginning includes the host stream tip.
        let page = session_ref.get_event_stream_since(None, 10).await.unwrap();
        assert_eq!(page.events.len(), 3);
        assert_eq!(page.latest_source_seq, 3);
        assert_eq!(page.events[0].seq, 1);
        assert_eq!(page.events[2].seq, 3);

        // Paging after a cursor returns only the strictly-later tail.
        let tail = session_ref
            .get_event_stream_since(Some(1), 10)
            .await
            .unwrap();
        assert_eq!(tail.events.len(), 2);
        assert_eq!(tail.events[0].seq, 2);
        assert_eq!(tail.latest_source_seq, 3);

        // At the tip the page is empty but the tip is still reported, which
        // is the loop-termination signal for backfill (§16.5).
        let at_tip = session_ref
            .get_event_stream_since(Some(3), 10)
            .await
            .unwrap();
        assert!(at_tip.events.is_empty());
        assert_eq!(at_tip.latest_source_seq, 3);
    }

    /// Owner (spawned actor) + attaching node (separate sink/journal) for the
    /// single-transfer sync tests.
    #[cfg(feature = "remote")]
    struct SyncRig {
        owner: ActorFixture,
        journal: Arc<dyn EventJournal>,
        sink: Arc<querymt_agent_services::event_sink::EventSink>,
        rx: tokio::sync::broadcast::Receiver<querymt_agent_core::events::EventEnvelope>,
    }

    #[cfg(feature = "remote")]
    impl SyncRig {
        async fn new(owner_events: usize) -> Self {
            let owner = ActorFixture::new().await;
            owner.append_owner_events(owner_events).await;
            let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .unwrap();
            let journal = storage.event_journal();
            let fanout = Arc::new(querymt_agent_services::event_fanout::EventFanout::new());
            let rx = fanout.subscribe();
            let sink = Arc::new(querymt_agent_services::event_sink::EventSink::new(
                journal.clone(),
                fanout,
            ));
            Self {
                owner,
                journal,
                sink,
                rx,
            }
        }

        fn sync(&self) -> crate::agent::remote::event_backfill::RemoteSync {
            crate::agent::remote::event_backfill::RemoteSync::new(
                self.sink.clone(),
                crate::agent::remote::SessionActorRef::from(self.owner.actor_ref.clone()),
                "test-session",
                "node-a",
                "peer-a",
            )
        }

        async fn local_positions(&self) -> Vec<i64> {
            self.journal
                .load_remote_history("test-session", "node-a")
                .await
                .unwrap()
                .into_iter()
                .map(|(position, _)| position)
                .collect()
        }

        async fn owner_tip(&self) -> i64 {
            self.owner
                .config
                .event_sink
                .journal()
                .max_stream_seq("test-session")
                .await
                .unwrap()
        }
    }

    #[cfg(feature = "remote")]
    impl ActorFixture {
        async fn append_owner_events(&self, count: usize) {
            let journal = self.config.event_sink.journal();
            for _ in 0..count {
                journal
                    .append_durable(&NewDurableEvent {
                        session_id: "test-session".into(),
                        origin: querymt_agent_core::events::EventOrigin::Local,
                        source_node: None,
                        source_node_id: None,
                        source_seq: None,
                        session_seq: None,
                        kind: querymt_agent_core::events::AgentEventKind::SessionCreated,
                    })
                    .await
                    .unwrap();
            }
        }
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn typical_remote_session_transfers_in_one_request_without_live_updates() {
        let mut rig = SyncRig::new(3_000).await;
        let sync = rig.sync();
        let tip = rig.owner_tip().await;
        sync.sync_all(false, None).await.unwrap();
        let (requests, received, inserted) = sync.state().transfers();
        assert_eq!(
            (requests, received, inserted),
            (1, tip as u64, tip as u64),
            "3,000 events take exactly one request"
        );
        assert_eq!(rig.local_positions().await, (1..=tip).collect::<Vec<_>>());
        // History is persisted without publishing.
        assert!(rig.rx.try_recv().is_err(), "no session/update for history");
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn tail_first_open_reads_in_owner_order_and_resumes_without_gaps() {
        let rig = SyncRig::new(50).await;
        let tip = rig.owner_tip().await;
        let sync = rig.sync().with_window_events(10);
        assert_eq!(sync.open(true).await.unwrap(), 10);
        // Only the covered recent window is visible, in owner order.
        assert_eq!(
            rig.local_positions().await,
            ((tip - 9)..=tip).collect::<Vec<_>>()
        );
        assert_eq!(
            rig.journal
                .remote_sync_cursor("test-session", "node-a")
                .await
                .unwrap(),
            Some(0),
            "cursor only advances over a contiguous prefix"
        );

        // Interrupted before the fill: a restarted node (new sink, same
        // journal) resumes from the persisted coverage.
        let restarted = Arc::new(querymt_agent_services::event_sink::EventSink::new(
            rig.journal.clone(),
            Arc::new(querymt_agent_services::event_fanout::EventFanout::new()),
        ));
        let resumed = crate::agent::remote::event_backfill::RemoteSync::new(
            restarted,
            crate::agent::remote::SessionActorRef::from(rig.owner.actor_ref.clone()),
            "test-session",
            "node-a",
            "peer-a",
        );
        assert_eq!(resumed.fill().await.unwrap(), Some((tip - 10) as usize));
        assert_eq!(rig.local_positions().await, (1..=tip).collect::<Vec<_>>());
        assert_eq!(
            rig.journal
                .remote_sync_cursor("test-session", "node-a")
                .await
                .unwrap(),
            Some(tip)
        );
        let stored = rig
            .journal
            .load_session_stream("test-session", None, None)
            .await
            .unwrap();
        assert_eq!(stored.len() as i64, tip, "no duplicates");
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn on_demand_and_fill_transfer_each_event_once() {
        let rig = SyncRig::new(200).await;
        let tip = rig.owner_tip().await;
        let sync = rig.sync().with_window_events(20);
        sync.open(true).await.unwrap();
        let (fill, demand_a, demand_b) = tokio::join!(
            sync.fill(),
            sync.ensure_covered(1, 100),
            sync.ensure_covered(50, 150)
        );
        fill.unwrap();
        demand_a.unwrap();
        demand_b.unwrap();
        let (_, received, inserted) = sync.state().transfers();
        assert_eq!(inserted, tip as u64);
        assert_eq!(received, tip as u64, "every event crossed the mesh once");
        assert_eq!(rig.local_positions().await, (1..=tip).collect::<Vec<_>>());
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn undone_turns_disappear_after_epoch_resync() {
        let rig = SyncRig::new(30).await;
        let sync = rig.sync();
        sync.sync_all(false, None).await.unwrap();
        assert_eq!(rig.local_positions().await.len(), 30);

        // Owner undoes events 21.. (bumps its history epoch), then continues.
        let owner_journal = rig.owner.config.event_sink.journal();
        owner_journal
            .delete_session_events_from("test-session", 21)
            .await
            .unwrap();
        rig.owner.append_owner_events(5).await;
        let tip = rig.owner_tip().await;

        sync.sync_all(false, None).await.unwrap();
        let positions = rig.local_positions().await;
        assert_eq!(positions.len() as i64, 20 + (tip - 30).max(0));
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(
            positions.len(),
            owner_journal
                .load_session_stream("test-session", None, None)
                .await
                .unwrap()
                .len(),
            "local copy equals the owner's rewritten history"
        );
        assert_eq!(sync.state().epoch(), Some(1));
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn local_copy_replaced_after_the_owners_undo_keeps_retained_charges_once() {
        use querymt_agent_core::accounting::samples::{sample_settlement, sample_start};
        use querymt_agent_core::events::AgentEventKind;
        use querymt_agent_services::accounting::SessionAccounting;

        let rig = SyncRig::new(0).await;
        let owner_journal = rig.owner.config.event_sink.journal();
        let append = |kind: AgentEventKind| {
            let journal = owner_journal.clone();
            async move {
                journal
                    .append_durable(&NewDurableEvent {
                        session_id: "test-session".into(),
                        origin: querymt_agent_core::events::EventOrigin::Local,
                        source_node: None,
                        source_node_id: None,
                        source_seq: None,
                        session_seq: None,
                        kind,
                    })
                    .await
                    .unwrap();
            }
        };
        // Two settled attempts the owner keeps (positions 1-4), then turns the
        // owner later undoes (positions 5-14).
        for id in ["att-1", "att-2"] {
            let mut start = sample_start();
            start.attempt.session_id = "test-session".into();
            start.attempt.attempt_id = id.into();
            start.attempt.operation_id = format!("op-{id}");
            let mut settlement = sample_settlement();
            settlement.attempt = start.attempt.clone();
            append(AgentEventKind::LlmAttemptStarted { start }).await;
            append(AgentEventKind::LlmAttemptSettled { settlement }).await;
        }
        rig.owner.append_owner_events(10).await;

        let sync = rig.sync();
        sync.sync_all(false, None).await.unwrap();
        let before = SessionAccounting::hydrate("test-session", rig.journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(before.priced_attempts, 2);

        // The owner undoes everything after its second attempt (bumping the
        // history epoch) and continues, so the attaching node replaces its copy.
        owner_journal
            .delete_session_events_from("test-session", 5)
            .await
            .unwrap();
        rig.owner.append_owner_events(3).await;
        sync.sync_all(false, None).await.unwrap();
        assert_eq!(sync.state().epoch(), Some(1), "local copy was replaced");

        let after = SessionAccounting::hydrate("test-session", rig.journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(after.priced_attempts, 2, "retained charges counted once");
        assert!((after.known_subtotal_usd - before.known_subtotal_usd).abs() < 1e-12);
        assert_eq!(after.unresolved_attempts, 0);

        let owner_totals = SessionAccounting::hydrate("test-session", owner_journal.clone())
            .await
            .unwrap()
            .read(|r| r.session().clone());
        assert_eq!(after.priced_attempts, owner_totals.priced_attempts);
        assert!((after.known_subtotal_usd - owner_totals.known_subtotal_usd).abs() < 1e-12);
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn reopen_transfers_only_new_events() {
        let rig = SyncRig::new(100).await;
        let sync = rig.sync();
        sync.sync_all(false, None).await.unwrap();

        let (_, before, _) = sync.state().transfers();
        // Reopen with no changes: no history events cross the mesh.
        assert_eq!(sync.open(false).await.unwrap(), 0);
        assert_eq!(sync.state().transfers().1, before);

        // 30 new events: exactly those 30 cross, on the same state (reused
        // attachment) and on a fresh node state after restart.
        rig.owner.append_owner_events(30).await;
        assert_eq!(sync.open(false).await.unwrap(), 30);
        assert_eq!(sync.state().transfers().1, before + 30);

        rig.owner.append_owner_events(30).await;
        let restarted = Arc::new(querymt_agent_services::event_sink::EventSink::new(
            rig.journal.clone(),
            Arc::new(querymt_agent_services::event_fanout::EventFanout::new()),
        ));
        let fresh = crate::agent::remote::event_backfill::RemoteSync::new(
            restarted,
            crate::agent::remote::SessionActorRef::from(rig.owner.actor_ref.clone()),
            "test-session",
            "node-a",
            "peer-a",
        );
        assert_eq!(fresh.open(false).await.unwrap(), 30);
        assert_eq!(fresh.state().transfers().1, 30);
        let tip = rig.owner_tip().await;
        assert_eq!(rig.local_positions().await, (1..=tip).collect::<Vec<_>>());
    }

    /// End-to-end backfill loop (plan §16) against a spawned session actor:
    /// legacy boundary skips the fetch, a persisted cursor recovers missed
    /// events exactly once, and re-running after reaching the tip is a no-op.
    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn backfill_recovers_missed_events_and_dedups_overlap() {
        use crate::agent::remote::event_backfill::RemoteSync;
        let fixture = ActorFixture::new().await;
        for _ in 0..3 {
            fixture
                .config
                .emit_event_persisted(
                    "test-session",
                    querymt_agent_core::events::AgentEventKind::SessionCreated,
                )
                .await
                .unwrap();
        }
        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let journal = storage.event_journal();
        let fanout = Arc::new(querymt_agent_services::event_fanout::EventFanout::new());
        let mut rx = fanout.subscribe();
        let sink = Arc::new(querymt_agent_services::event_sink::EventSink::new(
            journal.clone(),
            fanout,
        ));
        let session_ref = crate::agent::remote::SessionActorRef::from(fixture.actor_ref.clone());

        // Cursorless legacy rows remain stored, but the complete host snapshot
        // supersedes them in the remote read model without guessed identities.
        sink.emit_durable_with_origin(
            "test-session",
            querymt_agent_core::events::AgentEventKind::SessionCreated,
            querymt_agent_core::events::EventOrigin::Remote,
            Some("peer-a".into()),
        )
        .await
        .unwrap();
        rx.recv().await.unwrap();
        let cursor = journal
            .remote_sync_cursor("test-session", "node-a")
            .await
            .unwrap();
        assert_eq!(cursor, None);
        // Deterministically simulate a new live event arriving before backfill.
        sink.emit_durable_from_source(
            "test-session",
            querymt_agent_core::events::AgentEventKind::SessionCreated,
            Some("peer-a".into()),
            "node-a".into(),
            3,
            None,
        )
        .await
        .unwrap();
        rx.recv().await.unwrap();
        assert_eq!(
            journal
                .latest_source_seq("test-session", "node-a")
                .await
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            journal
                .remote_sync_cursor("test-session", "node-a")
                .await
                .unwrap(),
            None
        );

        RemoteSync::new(
            sink.clone(),
            session_ref.clone(),
            "test-session",
            "node-a",
            "peer-a",
        )
        .backfill(1, cursor, false, None)
        .await
        .unwrap();
        assert_eq!(
            journal
                .load_session_stream("test-session", None, None)
                .await
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            journal
                .load_remote_session_stream("test-session", "node-a")
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            journal
                .remote_sync_cursor("test-session", "node-a")
                .await
                .unwrap(),
            Some(3)
        );
        // History is persisted without publishing: only sync completion is
        // delivered (2 newly persisted events; the live overlap was a no-op).
        assert!(matches!(
            rx.recv().await.unwrap().kind(),
            querymt_agent_core::events::AgentEventKind::RemoteSessionSyncCompleted {
                backfilled: 2,
                boundary: true,
                ..
            }
        ));
        RemoteSync::new(sink, session_ref, "test-session", "node-a", "peer-a")
            .backfill(2, Some(3), false, None)
            .await
            .unwrap();
        assert!(matches!(
            rx.recv().await.unwrap().kind(),
            querymt_agent_core::events::AgentEventKind::RemoteSessionSyncCompleted {
                backfilled: 0,
                boundary: false,
                ..
            }
        ));
        assert!(rx.try_recv().is_err());
    }

    /// A relayed event whose host `session_seq` skips a number makes the relay
    /// fetch the missing durable events from the host before delivering the
    /// later one, so the attached node publishes the host's order.
    #[cfg(feature = "remote")]
    /// A loss at the very end of a burst has no later event to reveal it; the
    /// host forwarder's lag signal makes the relay fetch the durable tail.
    #[tokio::test]
    async fn relay_lag_signal_fetches_missed_tail_without_a_later_event() {
        use crate::agent::remote::event_relay::{EventRelayActor, RelayLagged, RelayedEvent};

        let host = ActorFixture::new().await;
        for _ in 0..4 {
            host.config
                .emit_event_persisted(
                    "test-session",
                    querymt_agent_core::events::AgentEventKind::SessionCreated,
                )
                .await
                .unwrap();
        }
        let host_events: Vec<querymt_agent_core::events::AgentEvent> = host
            .config
            .event_sink
            .journal()
            .load_session_stream("test-session", None, None)
            .await
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect();

        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let journal = storage.event_journal();
        let fanout = Arc::new(querymt_agent_services::event_fanout::EventFanout::new());
        let mut rx = fanout.subscribe();
        let sink = Arc::new(querymt_agent_services::event_sink::EventSink::new(
            journal.clone(),
            fanout,
        ));
        let relay = EventRelayActor::new(
            sink,
            "test-session".to_string(),
            "peer-a".to_string(),
            Some("node-a".to_string()),
            None,
        )
        .with_gap_source(crate::agent::remote::SessionActorRef::from(
            host.actor_ref.clone(),
        ));
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        // Only the first two events arrive; 3 and 4 were dropped by the host
        // forwarder and nothing later follows.
        for event in &host_events[..2] {
            relay_ref
                .tell(RelayedEvent {
                    event: event.clone(),
                })
                .await
                .unwrap();
        }
        relay_ref
            .tell(RelayLagged {
                session_id: "test-session".to_string(),
                skipped: 2,
            })
            .await
            .unwrap();
        // A signal for another session is ignored.
        relay_ref
            .tell(RelayLagged {
                session_id: "other-session".to_string(),
                skipped: 9,
            })
            .await
            .unwrap();

        let mut published = Vec::new();
        for _ in 0..4 {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("missed tail is fetched")
                .unwrap();
            published.push(envelope.session_seq());
        }
        assert_eq!(published, vec![1, 2, 3, 4]);
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn relayed_sequence_gap_is_fetched_from_host_before_later_events() {
        use crate::agent::remote::event_relay::{EventRelayActor, RelayedEvent};

        let host = ActorFixture::new().await;
        for _ in 0..4 {
            host.config
                .emit_event_persisted(
                    "test-session",
                    querymt_agent_core::events::AgentEventKind::SessionCreated,
                )
                .await
                .unwrap();
        }
        let host_events: Vec<querymt_agent_core::events::AgentEvent> = host
            .config
            .event_sink
            .journal()
            .load_session_stream("test-session", None, None)
            .await
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect();
        assert_eq!(
            host_events
                .iter()
                .map(|e| e.session_seq)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );

        let storage = crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
            .await
            .unwrap();
        let journal = storage.event_journal();
        let fanout = Arc::new(querymt_agent_services::event_fanout::EventFanout::new());
        let mut rx = fanout.subscribe();
        let sink = Arc::new(querymt_agent_services::event_sink::EventSink::new(
            journal.clone(),
            fanout,
        ));
        let relay = EventRelayActor::new(
            sink,
            "test-session".to_string(),
            "peer-a".to_string(),
            Some("node-a".to_string()),
            None,
        )
        .with_gap_source(crate::agent::remote::SessionActorRef::from(
            host.actor_ref.clone(),
        ));
        let relay_ref = <EventRelayActor as kameo::actor::Spawn>::spawn(relay);

        // Events 2 and 3 never reach the relay; 4 arrives after 1.
        for event in [&host_events[0], &host_events[3]] {
            relay_ref
                .tell(RelayedEvent {
                    event: event.clone(),
                })
                .await
                .unwrap();
        }

        let mut published = Vec::new();
        for _ in 0..4 {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("relayed event")
                .unwrap();
            published.push(envelope.session_seq());
        }
        assert_eq!(published, vec![1, 2, 3, 4], "host order, gap filled");
        let stored: Vec<i64> = journal
            .load_session_stream("test-session", None, None)
            .await
            .unwrap()
            .iter()
            .map(|e| e.session_seq)
            .collect();
        assert_eq!(stored, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn scheduled_prompt_with_unverifiable_peer_origin_cannot_run_as_owner() {
        use crate::session::domain_schedule::{ScheduleOrigin, SchedulePeerProtocol};

        let fixture = ActorFixture::new().await;
        let mut events = fixture.config.subscribe_events();
        let prompt = ScheduledPrompt {
            origin: ScheduleOrigin::Peer {
                host: "persisted-host".into(),
                peer: "persisted-peer".into(),
                protocol: SchedulePeerProtocol::V1,
            },
            ..scheduled_prompt()
        };
        let cycle_id = prompt.turn_id.to_string();
        let result = fixture.actor_ref.ask(prompt).await;
        assert!(matches!(
            result,
            Err(kameo::error::SendError::HandlerError(
                AgentError::MeshAccessDenied
            ))
        ));
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let AgentEventKind::ScheduledExecutionFailed {
                    schedule_public_id,
                    turn_id,
                    ..
                } = events.recv().await.unwrap().kind()
                    && schedule_public_id == "schedule-1"
                {
                    assert_eq!(turn_id.as_deref(), Some(cycle_id.as_str()));
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(fixture.run_lifecycle().await.is_empty());
        assert_eq!(fixture.history_writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn invalid_scheduled_task_does_not_write_history_or_intent() {
        let fixture = ActorFixture::new().await;
        let (_env, turn) = fixture.turn("test-session").await;
        let origin = ExecutionOrigin::Scheduled {
            schedule_public_id: "schedule-1".to_string(),
            task_public_id: "missing-task".to_string(),
        };

        let result = validate_execution_task(&*turn.state.store, &origin, "test-session").await;

        assert!(result.is_err());
        assert_eq!(fixture.history_writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.intent_writes.load(Ordering::SeqCst), 0);
        assert!(turn.state.current_intent.is_none());
    }

    #[tokio::test]
    async fn intent_projection_initializes_summary_without_next_step() {
        let fixture = ActorFixture::new().await;
        let (_env, mut turn) = fixture.turn("test-session").await;

        maybe_update_session_intent(
            &fixture.config,
            "test-session",
            &mut turn,
            "Implement the fix",
            "message-1",
        )
        .await
        .expect("update intent projection");

        let intent = turn.state.current_intent.expect("current intent");
        assert_eq!(intent.summary, "Implement the fix");
        assert_eq!(intent.next_step_hint, None);
        assert_eq!(fixture.intent_writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn normalized_run_instruction_preserves_text_beyond_16_kib() {
        let required_sentence = "REQUIRED: retain this instruction after the boundary.";
        let prompt = vec![crate::acp::protocol::ContentBlock::Text(
            crate::acp::protocol::TextContent::new(format!(
                "{}{}",
                "x".repeat(16 * 1024 + 1),
                required_sentence
            )),
        )];

        let instruction = normalized_run_instruction(&prompt);

        assert!(instruction.contains(required_sentence));
    }

    #[test]
    fn model_image_capability_allows_unknown_and_rejects_known_text_only() {
        let text_only = querymt::providers::ModelInfo {
            id: "text-only".to_string(),
            name: "Text only".to_string(),
            ..Default::default()
        };
        let multimodal = querymt::providers::ModelInfo {
            id: "vision".to_string(),
            name: "Vision".to_string(),
            capabilities: querymt::providers::ModelCapabilities {
                modalities: querymt::providers::Modalities {
                    input: vec!["text".to_string(), "image".to_string()],
                    output: vec!["text".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(model_supports_images(None));
        assert!(!model_supports_images(Some(&text_only)));
        assert!(model_supports_images(Some(&multimodal)));
    }

    #[tokio::test]
    async fn intent_projection_preserves_summary_constraints_and_updates_next_step() {
        let fixture = ActorFixture::new().await;
        let (_env, mut turn) = fixture.turn("test-session").await;
        turn.state.current_intent = Some(IntentSnapshot {
            id: 1,
            session_id: 1,
            task_id: None,
            summary: "Implement the fix".to_string(),
            constraints: Some("Keep API compatibility".to_string()),
            next_step_hint: None,
            revision: 1,
            source: "user_prompt".to_string(),
            source_ref: Some("message-1".to_string()),
            created_at: time::OffsetDateTime::now_utc(),
        });

        maybe_update_session_intent(
            &fixture.config,
            "test-session",
            &mut turn,
            "Also add integration tests",
            "message-2",
        )
        .await
        .expect("update intent projection");

        let intent = turn.state.current_intent.expect("current intent");
        assert_eq!(intent.summary, "Implement the fix");
        assert_eq!(
            intent.constraints.as_deref(),
            Some("Keep API compatibility")
        );
        assert_eq!(
            intent.next_step_hint.as_deref(),
            Some("Also add integration tests")
        );
        assert_eq!(fixture.intent_writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn intent_projection_skips_duplicate_summary_and_next_step() {
        let fixture = ActorFixture::new().await;
        let (_env, mut turn) = fixture.turn("test-session").await;
        turn.state.current_intent = Some(IntentSnapshot {
            id: 1,
            session_id: 1,
            task_id: None,
            summary: "Implement the fix".to_string(),
            constraints: None,
            next_step_hint: Some("Continue".to_string()),
            revision: 2,
            source: "user_prompt".to_string(),
            source_ref: Some("message-2".to_string()),
            created_at: time::OffsetDateTime::now_utc(),
        });

        for duplicate in ["Implement the fix", "Continue"] {
            maybe_update_session_intent(
                &fixture.config,
                "test-session",
                &mut turn,
                duplicate,
                "duplicate-message",
            )
            .await
            .expect("skip duplicate projection");
        }

        assert_eq!(fixture.intent_writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            turn.state
                .current_intent
                .as_ref()
                .and_then(|intent| intent.next_step_hint.as_deref()),
            Some("Continue")
        );
    }

    #[tokio::test]
    async fn provider_failure_recovers_contextualized_llm_error() {
        let fixture = ActorFixture::new().await;
        let (env, _turn) = fixture.turn("test-session").await;
        let llm_error = LLMError::from(
            ProviderFailure::new(ProviderErrorKind::ServerOverloaded, "provider is busy")
                .with_code(Some("server_overloaded".to_string()))
                .with_error_type(Some("api_error".to_string()))
                .with_request_id(Some("req-123".to_string()))
                .with_retry_after_secs(Some(3)),
        );
        let error = TurnError::Other(anyhow::Error::new(llm_error).context("execute cycle failed"));

        let provider_failure = provider_failure_from_execution_error(&error, env.llm_config())
            .expect("recover provider failure");

        match provider_failure {
            AgentError::ProviderFailure {
                message,
                provider,
                model,
                retryable,
                error,
            } => {
                assert_eq!(message, "execute cycle failed");
                assert_eq!(provider.as_deref(), Some("mock"));
                assert_eq!(model.as_deref(), Some("mock-model"));
                assert!(retryable);
                assert_eq!(
                    *error,
                    LLMErrorPayload::ProviderError {
                        message: "provider is busy".to_string(),
                        kind: Some(ProviderErrorKind::ServerOverloaded),
                        code: Some("server_overloaded".to_string()),
                        error_type: Some("api_error".to_string()),
                        request_id: Some("req-123".to_string()),
                        retry_after_secs: Some(3),
                    }
                );
            }
            other => panic!("expected provider failure, got {other:?}"),
        }
    }

    /// The variant the run itself produces (`contextualize_llm_error`): the
    /// failure reaches the client with the text the contextualised error had,
    /// and the provider's error as its payload.
    #[tokio::test]
    async fn provider_failure_classifies_the_typed_provider_error() {
        let fixture = ActorFixture::new().await;
        let (env, _turn) = fixture.turn("test-session").await;
        let llm_error = LLMError::from(ProviderFailure::new(
            ProviderErrorKind::ServerOverloaded,
            "provider is busy",
        ));
        // As `contextualize_llm_error` builds it.
        let error = TurnError::Provider {
            operation: "provider initialization",
            provider: env.provider_name().to_string(),
            model: env.model_name().to_string(),
            source: llm_error,
        };

        let provider_failure = provider_failure_from_execution_error(&error, env.llm_config())
            .expect("provider failure");

        match provider_failure {
            AgentError::ProviderFailure {
                message,
                provider,
                model,
                retryable,
                error: payload,
            } => {
                assert!(
                    message.starts_with(
                        "LLM provider initialization error (provider=mock, model=mock-model): "
                    ),
                    "{message}"
                );
                assert_eq!(provider.as_deref(), Some("mock"));
                assert_eq!(model.as_deref(), Some("mock-model"));
                assert!(retryable);
                assert!(matches!(
                    *payload,
                    LLMErrorPayload::ProviderError {
                        kind: Some(ProviderErrorKind::ServerOverloaded),
                        ..
                    }
                ));
            }
            other => panic!("expected provider failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provider_failure_ignores_non_llm_error_chain() {
        let fixture = ActorFixture::new().await;
        let (env, _turn) = fixture.turn("test-session").await;
        let error = TurnError::Other(
            anyhow::anyhow!("storage unavailable").context("execute cycle failed"),
        );

        assert!(provider_failure_from_execution_error(&error, env.llm_config()).is_none());
    }

    // ── Actor message handler tests ──────────────────────────────────────────

    #[tokio::test]
    async fn test_set_and_get_mode() {
        let f = ActorFixture::new().await;

        // Default mode is Build
        let mode = f.actor_ref.ask(GetMode).await.expect("ask GetMode");
        assert_eq!(mode, AgentMode::Build);

        // Switch to Plan
        f.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode");
        // Give the actor time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let mode = f.actor_ref.ask(GetMode).await.expect("ask GetMode");
        assert_eq!(mode, AgentMode::Plan);
    }

    #[tokio::test]
    async fn test_cancel_emits_event() {
        let f = ActorFixture::new().await;
        let mut rx = f.config.event_sink.fanout().subscribe();

        f.actor_ref.tell(Cancel).await.expect("tell Cancel");
        // Give the actor time to process
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        // Look for Cancelled event
        let mut found_cancel = false;
        while let Ok(envelope) = rx.try_recv() {
            if matches!(
                envelope.kind(),
                querymt_agent_core::events::AgentEventKind::Cancelled
            ) {
                found_cancel = true;
            }
        }
        assert!(found_cancel, "Expected Cancelled event on event fanout");
    }

    #[tokio::test]
    async fn last_phase_is_published_after_a_burst() {
        let fixture = ActorFixture::new().await;
        let mut actor = SessionActor::new(
            fixture.config.clone(),
            fixture._session_id.clone(),
            crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        );
        let run_id = querymt_agent_core::model::TurnId::new();
        actor.generation = 1;
        actor.run = RunSlot::Running(
            ActiveRun::new(run_id, 1, CancellationToken::new()),
            TurnAuthority::Local,
        );
        let actor_ref = SessionActor::spawn(actor);
        let mut events = fixture.config.subscribe_events();
        let (tx, rx) = tokio::sync::watch::channel(RunSignal::starting());
        let forwarder = tokio::spawn(forward_run_signals(rx, actor_ref.clone(), 1, run_id));

        // The run changes phase many times without yielding: the forwarder
        // cannot run in between, so it sees the latest value once.
        let burst = [RunPhase::Model, RunPhase::Tools, RunPhase::Waiting];
        for _ in 0..50 {
            for phase in burst {
                tx.send_modify(|signal| signal.phase = phase);
            }
        }
        tx.send_modify(|signal| signal.phase = RunPhase::Closing);

        let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let status = actor_ref.ask(GetRuntimeStatus).await.unwrap();
                if status.phase == SessionRuntimePhase::Closing {
                    break status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the last phase of the burst is published");
        assert_eq!(
            status.active_run_id.as_deref(),
            Some(run_id.to_string().as_str())
        );

        // What was published for the burst is its last phase, not each step.
        let mut published = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let AgentEventKind::RuntimeChanged { runtime } = event.kind() {
                published.push(runtime.phase);
            }
        }
        assert_eq!(published.last(), Some(&SessionRuntimePhase::Closing));
        assert!(
            published.len() < 10,
            "151 changes must coalesce, got {published:?}"
        );
        actor_ref.kill();
        actor_ref.wait_for_shutdown().await;
        forwarder.abort();
    }

    #[tokio::test]
    async fn status_consistency_steering_drain_publishes_new_revision_without_phase_change() {
        let fixture = crate::test_utils::TestAgent::new().await.with_inert_model();
        let (mut exec, mut turn) = fixture.turn().await;
        let mut actor = SessionActor::new(
            fixture.config.clone(),
            exec.session_id.clone(),
            exec.runtime.clone(),
        );
        actor.generation = 1;
        let held_run_id = querymt_agent_core::model::TurnId::new();
        let mut run = ActiveRun::new(held_run_id, 1, CancellationToken::new());
        run.phase = RunPhase::Model;
        exec.steering = Some(run.steering.clone());
        actor.run = RunSlot::Running(run, TurnAuthority::Local);
        let actor_ref = SessionActor::spawn(actor);
        let (tx, rx) = tokio::sync::watch::channel(RunSignal::starting());
        exec.phase_reporter = Some(tx);
        let forwarder = tokio::spawn(forward_run_signals(rx, actor_ref.clone(), 1, held_run_id));
        let mut events = fixture.config.subscribe_events();
        actor_ref
            .ask(SubmitSessionInput::new(SubmitInput {
                session_id: exec.session_id.clone(),
                client_input_id: Some("steer-once".into()),
                expected_run_id: Some(held_run_id.to_string()),
                delivery: InputDelivery::Steer,
                prompt: vec![crate::acp::protocol::ContentBlock::from("adjust task")],
            }))
            .await
            .unwrap();
        let before = actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(before.pending_steering_count, 1);
        crate::agent::execution::apply_pending_steering(
            &fixture.config,
            &exec,
            &mut turn,
            crate::agent::execution::SteeringBoundary::NextRequest,
            false,
        )
        .await
        .unwrap();
        let observation = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                if let AgentEventKind::RuntimeChanged { runtime } =
                    events.recv().await.unwrap().kind()
                    && runtime.pending_steering_count == 0
                {
                    break runtime.clone();
                }
            }
        })
        .await
        .expect("drain publishes runtime without another phase transition or read");
        assert_eq!(observation.active_run_id, before.active_run_id);
        assert!(observation.observation.revision > before.observation.revision);
        let after = actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(after.pending_steering_count, 0);
        assert!(after.observation.revision > before.observation.revision);
        actor_ref.kill();
        actor_ref.wait_for_shutdown().await;
        forwarder.abort();
    }

    #[tokio::test]
    async fn test_get_runtime_status_idle_when_not_running() {
        let f = ActorFixture::new().await;

        let status = f
            .actor_ref
            .ask(GetRuntimeStatus)
            .await
            .expect("ask GetRuntimeStatus");

        assert_eq!(status.phase, SessionRuntimePhase::Idle);
    }

    #[tokio::test]
    async fn test_get_runtime_status_remains_idle_after_cancel_without_running_prompt() {
        let f = ActorFixture::new().await;

        f.actor_ref.tell(Cancel).await.expect("tell Cancel");

        let status = f
            .actor_ref
            .ask(GetRuntimeStatus)
            .await
            .expect("ask GetRuntimeStatus");

        assert_eq!(status.phase, SessionRuntimePhase::Idle);
    }

    #[tokio::test]
    async fn test_get_runtime_status_cancel_requested_when_prompt_running_and_cancelled() {
        let f = ActorFixture::new().await;

        // Simulate prompt_running=true and token cancelled (the state after Cancel
        // is received while a prompt execution task is still in flight).
        f.actor_ref
            .tell(PromptFinished {
                generation: 0,
                run_id: querymt_agent_core::model::TurnId::new(),
                disposition: RunCompletionDisposition::Completed,
            })
            .await
            .expect("tell PromptFinished");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Now set prompt_running=true and cancel the token
        f.actor_ref.tell(Cancel).await.expect("tell Cancel");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // We can't directly set prompt_running from outside, but we can verify
        // the status logic: when prompt_running=false and token is cancelled,
        // status should be Idle (not CancelRequested) because there's no running
        // prompt to cancel.
        let status = f
            .actor_ref
            .ask(GetRuntimeStatus)
            .await
            .expect("ask GetRuntimeStatus");

        // prompt_running is false (no prompt was sent), so status is Idle
        // even though the token was cancelled.
        assert_eq!(status.phase, SessionRuntimePhase::Idle);
    }

    #[tokio::test]
    async fn test_set_mode_emits_event() {
        let f = ActorFixture::new().await;
        let mut rx = f.config.event_sink.fanout().subscribe();

        f.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut found = false;
        while let Ok(envelope) = rx.try_recv() {
            if let querymt_agent_core::events::AgentEventKind::SessionModeChanged { mode } =
                envelope.kind()
            {
                assert_eq!(*mode, AgentMode::Plan);
                found = true;
            }
        }
        assert!(found, "Expected SessionModeChanged event");
    }

    #[tokio::test]
    async fn test_mode_change_emits_config_snapshot_once() {
        let f = ActorFixture::new().await;
        let mut rx = f.config.event_sink.fanout().subscribe();

        f.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut snapshots = 0;
        while let Ok(envelope) = rx.try_recv() {
            if let querymt_agent_core::events::AgentEventKind::SessionConfigChanged {
                mode, ..
            } = envelope.kind()
            {
                assert_eq!(*mode, AgentMode::Plan);
                snapshots += 1;
            }
        }
        assert_eq!(snapshots, 1, "expected one config snapshot for the change");
    }

    #[tokio::test]
    async fn test_noop_mode_change_emits_no_config_snapshot() {
        let f = ActorFixture::new().await;
        // Establish the committed control state first.
        f.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let mut rx = f.config.event_sink.fanout().subscribe();
        // Same mode again: no effective change, so no snapshot.
        f.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode");
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        while let Ok(envelope) = rx.try_recv() {
            assert!(
                !matches!(
                    envelope.kind(),
                    querymt_agent_core::events::AgentEventKind::SessionConfigChanged { .. }
                        | querymt_agent_core::events::AgentEventKind::SessionModeChanged { .. }
                ),
                "no-op transitions must not emit configuration updates"
            );
        }
    }

    #[tokio::test]
    async fn test_set_provider_unknown_fails() {
        let f = ActorFixture::new().await;
        let result = f
            .actor_ref
            .ask(SetProvider {
                provider: "nonexistent-provider".to_string(),
                model: "some-model".to_string(),
            })
            .await;

        assert!(
            result.is_err(),
            "expected error for unknown provider, got ok"
        );
    }

    #[tokio::test]
    async fn test_set_tool_policy() {
        let f = ActorFixture::new().await;

        f.actor_ref
            .tell(SetToolPolicy {
                policy: ToolPolicy::BuiltInOnly,
            })
            .await
            .expect("tell SetToolPolicy");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Verify the policy was set by checking GetLlmConfig (indirect; we can't read tool_config directly)
        // Instead: setting allowed tools should persist without error
        f.actor_ref
            .tell(SetAllowedTools {
                tools: vec!["shell".parse().unwrap(), "read_tool".parse().unwrap()],
            })
            .await
            .expect("tell SetAllowedTools");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // Clear should also succeed
        f.actor_ref
            .tell(ClearAllowedTools)
            .await
            .expect("tell ClearAllowedTools");
    }

    /// Test-only probe for the session's live tool lists.
    struct ProbeToolConfig;

    #[derive(kameo::Reply)]
    struct ProbedToolConfig(ToolConfig);

    impl Message<ProbeToolConfig> for SessionActor {
        type Reply = ProbedToolConfig;

        async fn handle(
            &mut self,
            _msg: ProbeToolConfig,
            _ctx: &mut Context<Self, Self::Reply>,
        ) -> Self::Reply {
            ProbedToolConfig(self.tool_config.clone())
        }
    }

    #[tokio::test]
    async fn runtime_deny_uses_the_shared_selector_rule() {
        use crate::config::ToolRef;
        let f = ActorFixture::new().await;
        f.actor_ref
            .tell(SetDeniedTools {
                tools: vec!["github.*".parse().unwrap(), "lookup".parse().unwrap()],
            })
            .await
            .expect("tell SetDeniedTools");
        let ProbedToolConfig(config) = f.actor_ref.ask(ProbeToolConfig).await.expect("probe");
        // `github.*` hides every github tool.
        assert!(!config.allows(ToolRef::mcp("github", "create_issue")));
        // A bare `lookup` deny hides the built-in but keeps `alpha.lookup`.
        assert!(!config.allows(ToolRef::builtin("lookup")));
        assert!(config.allows(ToolRef::mcp("alpha", "lookup")));
    }

    #[tokio::test]
    async fn malformed_selector_leaves_session_lists_unchanged() {
        use crate::config::ToolRef;
        let f = ActorFixture::new().await;
        f.actor_ref
            .tell(SetDeniedTools {
                tools: vec!["shell".parse().unwrap()],
            })
            .await
            .expect("tell SetDeniedTools");
        // A remote payload with a malformed selector cannot be decoded, so no
        // message reaches the actor.
        assert!(serde_json::from_str::<SetDeniedTools>(r#"{"tools":["github."]}"#).is_err());
        let ProbedToolConfig(config) = f.actor_ref.ask(ProbeToolConfig).await.expect("probe");
        assert!(!config.allows(ToolRef::builtin("shell")));
        assert!(config.allows(ToolRef::builtin("read_tool")));
    }

    #[tokio::test]
    async fn test_set_denied_tools() {
        let f = ActorFixture::new().await;

        f.actor_ref
            .tell(SetDeniedTools {
                tools: vec!["shell".parse().unwrap()],
            })
            .await
            .expect("tell SetDeniedTools");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        f.actor_ref
            .tell(ClearDeniedTools)
            .await
            .expect("tell ClearDeniedTools");
    }

    #[tokio::test]
    async fn test_get_session_limits_returns_ok() {
        let f = ActorFixture::new().await;
        let limits = f
            .actor_ref
            .ask(GetSessionLimits)
            .await
            .expect("ask GetSessionLimits");
        // No limits middleware configured, so None is correct
        assert!(limits.is_none());
    }

    #[tokio::test]
    async fn test_get_llm_config() {
        let f = ActorFixture::new().await;
        let result = f
            .actor_ref
            .ask(GetLlmConfig)
            .await
            .expect("ask GetLlmConfig");
        // The mock store returns a config — None is fine (mock may not set it)
        // Just verify no error occurred (already asserted by .expect above)
        drop(result);
    }

    #[tokio::test]
    async fn test_set_reasoning_effort_allows_remote_only_provider() {
        let provider = Arc::new(Mutex::new(MockLlmProvider::new()));
        let shared = SharedLlmProvider {
            inner: provider,
            tools: vec![].into_boxed_slice(),
        };
        let factory = Arc::new(TestProviderFactory::new(shared));
        let (plugin_registry, _temp_dir) = mock_plugin_registry(factory).expect("plugin registry");

        let session = mock_session("remote-session");
        let remote_config = crate::session::store::LLMConfig {
            id: 42,
            name: None,
            provider: "codex".to_string(),
            model: "gpt-5.4".to_string(),
            protocol: String::new(),
            endpoint: String::new(),
            params: Some(serde_json::json!({
                "reasoning_effort": "medium"
            })),
            created_at: Some(time::OffsetDateTime::now_utc()),
            updated_at: Some(time::OffsetDateTime::now_utc()),
            provider_node_id: Some("12D3KooWRemoteNode".to_string()),
        };

        let mut store = MockSessionStore::new();
        let session_clone = session.clone();
        store
            .expect_get_session()
            .returning(move |_| Ok(Some(session_clone.clone())))
            .times(0..);
        let remote_config_for_get = remote_config.clone();
        store
            .expect_get_session_llm_config()
            .returning(move |_| Ok(Some(remote_config_for_get.clone())))
            .times(1..);
        store
            .expect_get_session_control()
            .returning(|_| Ok(None))
            .times(1);
        store
            .expect_commit_session_control()
            .returning(|_, _, state| Ok(state.clone()))
            .times(2);
        let remote_config_for_id = remote_config.clone();
        store
            .expect_get_llm_config()
            .returning(move |_| Ok(Some(remote_config_for_id.clone())))
            .times(0..);
        store
            .expect_get_session_provider_node_id()
            .withf(|session_id| session_id == "remote-session")
            .returning(|_| Ok(Some("12D3KooWRemoteNode".to_string())))
            .times(1..);
        store
            .expect_create_or_get_llm_config()
            .withf(|config| {
                config.provider.as_deref() == Some("codex")
                    && config.model.as_deref() == Some("gpt-5.4")
                    && config.reasoning_effort == Some(ReasoningEffort::High)
            })
            .returning(|_| {
                Ok(crate::session::store::LLMConfig {
                    id: 43,
                    name: None,
                    provider: "codex".to_string(),
                    model: "gpt-5.4".to_string(),
                    protocol: String::new(),
                    endpoint: String::new(),
                    params: Some(serde_json::json!({"reasoning_effort": "high"})),
                    created_at: Some(time::OffsetDateTime::now_utc()),
                    updated_at: Some(time::OffsetDateTime::now_utc()),
                    provider_node_id: Some("12D3KooWRemoteNode".to_string()),
                })
            })
            .times(3);
        store
            .expect_set_session_llm_config()
            .returning(|_, _| Ok(()))
            .times(0);
        store
            .expect_set_session_provider_node_id()
            .returning(|_, _| Ok(()))
            .times(0..);
        store
            .expect_list_sessions()
            .returning(|| Ok(vec![]))
            .times(0..);

        let storage = Arc::new(
            crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                .await
                .expect("create event store"),
        );

        let store: Arc<dyn SessionStore> = Arc::new(store);
        let provider = Arc::new(crate::session::provider::SessionProvider::new(
            Arc::new(plugin_registry),
            store,
            LLMParams::new().provider("mock").model("mock-model"),
        ));
        let config = Arc::new(
            AgentConfigBuilder::from_provider(storage.clone(), provider, storage.event_journal())
                .with_tool_policy(ToolPolicy::ProviderOnly)
                .build(),
        );

        let runtime = crate::agent::core::SessionRuntime::new(
            None,
            HashMap::new(),
            crate::agent::core::McpToolState::empty(),
        );
        let actor = SessionActor::new(config, "remote-session".to_string(), runtime);
        let actor_ref = SessionActor::spawn(actor);

        actor_ref
            .ask(SetReasoningEffort {
                effort: Some(ReasoningEffort::High),
            })
            .await
            .expect("remote session should accept reasoning effort update");
    }

    async fn register_test_interaction(
        f: &ActorFixture,
        id: &str,
        session_id: &str,
    ) -> tokio::sync::oneshot::Receiver<crate::elicitation::ElicitationResponse> {
        let (tx, rx) = oneshot::channel();
        crate::elicitation::register_pending_elicitation(
            &f.config.pending_elicitations,
            id.to_string(),
            crate::elicitation::PendingElicitationRegistration {
                session_id: session_id.to_string(),
                form: crate::elicitation::PendingElicitationForm {
                    message: "pick".to_string(),
                    requested_schema: serde_json::json!({
                        "type": "object",
                        "properties": {"choice": {"type": "string"}},
                        "required": ["choice"]
                    }),
                    source: "test".to_string(),
                },
                owner_authority: None,
                run_id: None,
                tool_call_id: None,
            },
            tx,
        )
        .await;
        rx
    }

    fn accept(content: serde_json::Value) -> crate::elicitation::ElicitationResponse {
        crate::elicitation::ElicitationResponse {
            action: crate::elicitation::ElicitationAction::Accept,
            content: Some(content),
        }
    }

    #[cfg(feature = "remote")]
    struct PausedInteractionGrantStore {
        started: Semaphore,
        resume: Semaphore,
        changes: broadcast::Sender<PolicyChange>,
    }

    #[cfg(feature = "remote")]
    #[async_trait::async_trait]
    impl GrantStore for PausedInteractionGrantStore {
        async fn grant(&self, _: &PeerGrantKey) -> Result<PeerGrantState, GrantStoreError> {
            self.started.add_permits(1);
            self.resume.acquire().await.unwrap().forget();
            Ok(PeerGrantState {
                grant: PeerGrant::Full,
                ..Default::default()
            })
        }

        fn subscribe_changes(&self) -> Option<broadcast::Receiver<PolicyChange>> {
            Some(self.changes.subscribe())
        }

        async fn replace_grant(
            &self,
            _: &PeerGrantKey,
            _: Uuid,
            _: PeerGrant,
        ) -> Result<PeerGrantState, GrantStoreError> {
            unreachable!("test delivers a policy effect directly")
        }

        async fn managed_peers(&self, _: &NodeId) -> Result<Vec<NodeId>, GrantStoreError> {
            unreachable!("interaction handling does not list peers")
        }
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn peer_interaction_policy_reads_leave_map_free_and_revocation_blocks_use() {
        let f = ActorFixture::new().await;
        let store = Arc::new(PausedInteractionGrantStore {
            started: Semaphore::new(0),
            resume: Semaphore::new(0),
            changes: broadcast::channel(1).0,
        });
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(libp2p::PeerId::random()),
            peer: NodeId::from_peer_id(libp2p::PeerId::random()),
        };
        let peer = PeerTurn::test_peer(key.clone(), PeerGrant::Full, store.clone());
        let mut actor = SessionActor::new(
            f.config.clone(),
            "peer-session".into(),
            SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        );
        let run_id = querymt_agent_core::model::TurnId::new();
        actor.run = RunSlot::Running(
            ActiveRun::new(run_id, 1, CancellationToken::new()),
            TurnAuthority::Peer(peer.clone()),
        );
        let actor_ref = SessionActor::spawn(actor);
        let ask = tokio::spawn(async move {
            actor_ref
                .ask(PeerInteraction {
                    peer,
                    message: ListPendingInteractions {
                        assign_authority: Some("verifier".into()),
                    },
                })
                .await
        });

        store.started.acquire().await.unwrap().forget();
        let entries = f
            .config
            .pending_elicitations
            .try_lock()
            .expect("another session can access the map during grant I/O");
        store.resume.add_permits(2);
        store.started.acquire().await.unwrap().forget();
        // Both reads report the earlier Full policy. Its revocation is observed at use.
        store
            .changes
            .send(PolicyChange {
                key,
                effect: PolicyEffect::Grant(PeerGrant::None),
            })
            .unwrap();
        drop(entries);
        assert!(matches!(
            ask.await.unwrap(),
            Err(kameo::error::SendError::HandlerError(
                AgentError::MeshAccessDenied
            ))
        ));
    }

    #[tokio::test]
    async fn interaction_messages_claim_generation_and_single_winner_resolve() {
        use crate::elicitation::ElicitationResolution;
        let f = ActorFixture::new().await;
        let rx = register_test_interaction(&f, "q1", "test-session").await;
        // Entries of other sessions are never listed or claimed by this actor.
        let _other = register_test_interaction(&f, "q-other", "other-session").await;

        let listed = f
            .actor_ref
            .ask(ListPendingInteractions {
                assign_authority: Some("auth".to_string()),
            })
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].elicitation_id, "q1");
        assert_eq!(listed[0].owner_authority.as_deref(), Some("auth"));

        let claim = |connection: &str| ClaimInteractionDelivery {
            interaction_id: Some("q1".to_string()),
            connection_id: connection.to_string(),
            authority: Some("auth".to_string()),
        };
        let first = f.actor_ref.ask(claim("conn-a")).await.expect("claim a");
        assert_eq!(first[0].delivery_generation, 1);
        let second = f.actor_ref.ask(claim("conn-b")).await.expect("claim b");
        assert_eq!(second[0].delivery_generation, 2);

        let resolve =
            |connection: &str, generation: u64, content: serde_json::Value| ResolveInteraction {
                interaction_id: "q1".to_string(),
                connection_id: connection.to_string(),
                delivery_generation: Some(generation),
                response: accept(content),
            };
        let ok = serde_json::json!({"choice": "x"});
        // Stale generation and wrong connection do not resolve.
        for stale in [
            resolve("conn-a", 1, ok.clone()),
            resolve("conn-a", 2, ok.clone()),
            resolve("conn-b", 1, ok.clone()),
        ] {
            assert_eq!(
                f.actor_ref.ask(stale).await.expect("stale"),
                ElicitationResolution::StaleDelivery
            );
        }
        // Invalid form content is rejected and the question stays pending.
        let invalid = f
            .actor_ref
            .ask(resolve("conn-b", 2, serde_json::json!({})))
            .await;
        assert!(matches!(
            invalid,
            Err(kameo::error::SendError::HandlerError(
                AgentError::InvalidInteractionResponse { .. }
            ))
        ));

        assert_eq!(
            f.actor_ref
                .ask(resolve("conn-b", 2, ok.clone()))
                .await
                .expect("resolve"),
            ElicitationResolution::Resolved
        );
        assert_eq!(rx.await.expect("waiter resumed").content, Some(ok.clone()));
        // A second answer is not pending any more.
        assert_eq!(
            f.actor_ref
                .ask(resolve("conn-b", 2, ok))
                .await
                .expect("late"),
            ElicitationResolution::NotFound
        );
    }

    #[tokio::test]
    async fn test_ext_method_returns_null() {
        let f = ActorFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        let result = f
            .actor_ref
            .ask(ExtMethod {
                req: crate::acp::protocol::ExtRequest::new("custom_method", null_params),
            })
            .await
            .expect("ask ExtMethod");
        // The default implementation returns a null JSON value
        assert_eq!(result.0.get(), "null");
    }

    #[tokio::test]
    async fn test_ext_notification_succeeds() {
        let f = ActorFixture::new().await;
        let null_params = std::sync::Arc::from(
            serde_json::value::RawValue::from_string("null".to_string()).unwrap(),
        );
        f.actor_ref
            .ask(ExtNotification {
                notif: crate::acp::protocol::ExtNotification::new("my_notification", null_params),
            })
            .await
            .expect("ask ExtNotification");
    }

    #[tokio::test]
    async fn test_subscribe_unsubscribe_events_no_panic() {
        let f = ActorFixture::new().await;
        let result = f
            .actor_ref
            .ask(crate::agent::messages::SubscribeEvents {
                relay_actor_id: 42,
                #[cfg(feature = "remote")]
                relay_ref: None,
                relay_dht_name: Some("event_relay::test::peer-X".to_string()),
                after_source_seq: None,
            })
            .await;
        #[cfg(feature = "remote")]
        assert!(matches!(
            result,
            Err(kameo::error::SendError::HandlerError(
                AgentError::MeshNotBootstrapped
            ))
        ));
        #[cfg(not(feature = "remote"))]
        result.expect("ask SubscribeEvents");

        f.actor_ref
            .ask(crate::agent::messages::UnsubscribeEvents {
                relay_actor_id: 42,
                relay_dht_name: "event_relay::test::peer-X".to_string(),
            })
            .await
            .expect("ask UnsubscribeEvents");
    }

    #[tokio::test]
    async fn test_prompt_finished_resets_state() {
        let f = ActorFixture::new().await;
        // Send PromptFinished — even when not "running", should be a no-op that doesn't panic
        f.actor_ref
            .tell(PromptFinished {
                generation: 0,
                run_id: querymt_agent_core::model::TurnId::new(),
                disposition: RunCompletionDisposition::Completed,
            })
            .await
            .expect("tell PromptFinished");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // GetMode should still work, confirming actor is alive
        let mode = f.actor_ref.ask(GetMode).await.expect("ask GetMode");
        assert_eq!(mode, AgentMode::Build);
    }

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        assert_eq!(panic_message(&"literal"), "literal");
        assert_eq!(panic_message(&String::from("formatted 1")), "formatted 1");
        assert_eq!(panic_message(&42_u32), "unknown panic");
    }

    #[test]
    fn run_disposition_reports_stops_without_touching_the_response() {
        let stopped: Result<PromptRun, AgentError> = Ok(PromptRun {
            response: PromptResponse::new(StopReason::MaxTokens),
            stop: Some((
                querymt_agent_core::events::StopType::ModelTokenLimit,
                "cut off".into(),
            )),
        });
        assert_eq!(
            run_disposition(false, &stopped),
            RunCompletionDisposition::Stopped {
                stop_type: querymt_agent_core::events::StopType::ModelTokenLimit,
                reason: "cut off".into(),
            }
        );
        // Cancellation wins over a stop; plain completion and errors map as before.
        assert_eq!(
            run_disposition(true, &stopped),
            RunCompletionDisposition::Cancelled
        );
        let done: Result<PromptRun, AgentError> = Ok(PromptRun::finished(PromptResponse::new(
            StopReason::EndTurn,
        )));
        assert_eq!(
            run_disposition(false, &done),
            RunCompletionDisposition::Completed
        );
        let failed: Result<PromptRun, AgentError> = Err(AgentError::Internal("x".into()));
        assert_eq!(
            run_disposition(false, &failed),
            RunCompletionDisposition::Failed
        );
    }

    #[tokio::test]
    async fn prompt_finished_with_a_stop_emits_run_completed_stopped() {
        let fixture = ActorFixture::new().await;
        let mut actor = SessionActor::new(
            fixture.config.clone(),
            fixture._session_id.clone(),
            crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        );
        let stop_run_id = querymt_agent_core::model::TurnId::new();
        actor.run = RunSlot::Running(
            ActiveRun::new(stop_run_id, 1, CancellationToken::new()),
            TurnAuthority::Local,
        );
        let actor_ref = SessionActor::spawn(actor);
        let mut events = fixture.config.subscribe_events();

        actor_ref
            .tell(PromptFinished {
                generation: 1,
                run_id: stop_run_id,
                disposition: RunCompletionDisposition::Stopped {
                    stop_type: querymt_agent_core::events::StopType::ModelTokenLimit,
                    reason: "output limit".to_string(),
                },
            })
            .await
            .expect("tell PromptFinished");

        let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let envelope = events.recv().await.expect("event stream open");
                if let AgentEventKind::RunCompleted {
                    outcome,
                    stop_type,
                    reason,
                    ..
                } = envelope.kind()
                {
                    return (outcome.clone(), *stop_type, reason.clone());
                }
            }
        })
        .await
        .expect("run_completed emitted");
        assert_eq!(completed.0, "stopped");
        assert_eq!(
            completed.1,
            Some(querymt_agent_core::events::StopType::ModelTokenLimit)
        );
        assert_eq!(completed.2.as_deref(), Some("output limit"));
    }

    #[tokio::test]
    async fn discard_queued_input_removes_only_pending_input_and_is_idempotent() {
        let fixture = ActorFixture::new().await;
        let session_id = fixture._session_id.clone();
        let mut actor = SessionActor::new(
            fixture.config.clone(),
            session_id.clone(),
            crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        );
        actor.run = RunSlot::Running(
            ActiveRun::new(
                querymt_agent_core::model::TurnId::new(),
                1,
                CancellationToken::new(),
            ),
            TurnAuthority::Local,
        );
        let actor_ref = SessionActor::spawn(actor);
        let mut events = fixture.config.subscribe_events();
        for input_id in ["queued-1", "queued-2"] {
            actor_ref
                .ask(SubmitSessionInput::new(SubmitInput {
                    session_id: session_id.clone(),
                    client_input_id: Some(input_id.to_string()),
                    expected_run_id: None,
                    delivery: InputDelivery::Queue,
                    prompt: vec![crate::acp::protocol::ContentBlock::from(input_id)],
                }))
                .await
                .expect("queue input");
        }

        let discarded = actor_ref
            .ask(DiscardQueuedInput::new("queued-1".to_string()))
            .await
            .expect("discard queued input");
        assert_eq!(
            discarded,
            DiscardQueuedInputResult::Discarded {
                input_id: "queued-1".to_string()
            }
        );
        assert_eq!(
            actor_ref
                .ask(GetRuntimeStatus)
                .await
                .expect("get runtime status")
                .queued_input_count,
            1
        );

        let repeated = actor_ref
            .ask(DiscardQueuedInput::new("queued-1".to_string()))
            .await
            .expect("repeat discard queued input");
        assert_eq!(
            repeated,
            DiscardQueuedInputResult::NotPending {
                input_id: "queued-1".to_string()
            }
        );

        let mut saw_queued_content = false;
        let mut saw_discarded = false;
        while !(saw_queued_content && saw_discarded) {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                .await
                .expect("queue lifecycle event")
                .expect("event stream open");
            match event.kind() {
                AgentEventKind::InputQueued {
                    input_id,
                    blocks,
                    accepted_at_ms,
                    ..
                } if input_id == "queued-1" => {
                    assert_eq!(
                        serde_json::to_value(blocks).expect("serialize queued blocks"),
                        serde_json::json!([{"type": "text", "text": "queued-1"}])
                    );
                    assert!(accepted_at_ms.is_some());
                    saw_queued_content = true;
                }
                AgentEventKind::QueuedInputDiscarded { input_id, reason }
                    if input_id == "queued-1" && reason == "removed_by_user" =>
                {
                    assert!(event.is_durable());
                    saw_discarded = true;
                }
                _ => {}
            }
        }
        assert!(saw_queued_content, "queued input content event missing");
        assert!(saw_discarded, "queued discard lifecycle event missing");
        let durable_kinds = fixture
            .event_journal
            .load_session_stream(&session_id, None, None)
            .await
            .expect("load queued input lifecycle")
            .into_iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>();
        let queued_index = durable_kinds
            .iter()
            .position(|kind| {
                matches!(
                    kind,
                    AgentEventKind::InputQueued { input_id, .. } if input_id == "queued-1"
                )
            })
            .expect("queued event should be durable");
        let discarded_index = durable_kinds
            .iter()
            .position(|kind| {
                matches!(
                    kind,
                    AgentEventKind::QueuedInputDiscarded { input_id, .. }
                        if input_id == "queued-1"
                )
            })
            .expect("discard event should be durable");
        assert!(queued_index < discarded_index);
        actor_ref.tell(Shutdown).await.expect("shutdown actor");
    }

    #[tokio::test]
    async fn queued_input_mutations_wait_for_durable_events() {
        let fixture = ActorFixture::new().await;
        let session_id = fixture._session_id.clone();
        let mut actor = SessionActor::new(
            fixture.config.clone(),
            session_id.clone(),
            crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        );
        actor.run = RunSlot::Running(
            ActiveRun::new(
                querymt_agent_core::model::TurnId::new(),
                1,
                CancellationToken::new(),
            ),
            TurnAuthority::Local,
        );
        let actor_ref = SessionActor::spawn(actor);
        let submit = || {
            SubmitSessionInput::new(SubmitInput {
                session_id: session_id.clone(),
                client_input_id: Some("queued-failure".to_string()),
                expected_run_id: None,
                delivery: InputDelivery::Queue,
                prompt: vec![crate::acp::protocol::ContentBlock::from("queued")],
            })
        };

        fixture.event_journal.fail_next_append();
        let failed_queue = actor_ref.ask(submit()).await;
        assert!(matches!(
            failed_queue,
            Err(kameo::error::SendError::HandlerError(AgentError::Internal(message)))
                if message.contains("simulated event append failure")
        ));
        assert_eq!(
            actor_ref
                .ask(GetRuntimeStatus)
                .await
                .expect("get status after failed queue")
                .queued_input_count,
            0
        );

        let queued = actor_ref
            .ask(submit())
            .await
            .expect("queue retry should succeed");
        assert!(matches!(
            queued,
            SubmitInputResult::Queued { ref input_id, position: 1 }
                if input_id == "queued-failure"
        ));

        fixture.event_journal.fail_next_append();
        let failed_discard = actor_ref
            .ask(DiscardQueuedInput::new("queued-failure".to_string()))
            .await;
        assert!(matches!(
            failed_discard,
            Err(kameo::error::SendError::HandlerError(AgentError::Internal(message)))
                if message.contains("simulated event append failure")
        ));
        assert_eq!(
            actor_ref
                .ask(GetRuntimeStatus)
                .await
                .expect("get status after failed discard")
                .queued_input_count,
            1
        );

        let discarded = actor_ref
            .ask(DiscardQueuedInput::new("queued-failure".to_string()))
            .await
            .expect("discard retry should succeed");
        assert_eq!(
            discarded,
            DiscardQueuedInputResult::Discarded {
                input_id: "queued-failure".to_string()
            }
        );
        assert_eq!(
            actor_ref
                .ask(GetRuntimeStatus)
                .await
                .expect("get status after discard retry")
                .queued_input_count,
            0
        );
        actor_ref.tell(Shutdown).await.expect("shutdown actor");
    }

    #[tokio::test]
    async fn test_shutdown_stops_actor() {
        let f = ActorFixture::new().await;
        // Shutdown the actor
        f.actor_ref.tell(Shutdown).await.expect("tell Shutdown");
        // Give the actor time to stop
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        // After shutdown, the actor should no longer accept messages
        let result = f.actor_ref.ask(GetMode).await;
        assert!(result.is_err(), "actor should be stopped after Shutdown");
    }

    #[tokio::test]
    async fn shutdown_releases_run_lease_before_returning() {
        let f = ActorFixture::new().await;
        let started = f
            .actor_ref
            .ask(text_submit(&f._session_id, "hi"))
            .await
            .expect("submit");
        assert!(matches!(started, SubmitInputResult::Started { .. }));

        f.actor_ref.tell(Shutdown).await.expect("tell Shutdown");
        f.actor_ref
            .wait_for_shutdown_result()
            .await
            .expect("actor shut down");

        // The lease is dropped in on_stop, so it is free as soon as shutdown
        // is reported (a delete can then remove the lease file).
        assert!(
            f.lease_root()
                .try_acquire(&f._session_id)
                .unwrap()
                .is_some(),
            "stopped actor must not keep the run lease"
        );
    }

    #[tokio::test]
    async fn test_multiple_sessions_independent() {
        let f1 = ActorFixture::with_session_id("session-alpha").await;
        let f2 = ActorFixture::with_session_id("session-beta").await;

        f1.actor_ref
            .tell(SetMode {
                mode: AgentMode::Plan,
            })
            .await
            .expect("tell SetMode on f1");
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let mode1 = f1.actor_ref.ask(GetMode).await.expect("GetMode f1");
        let mode2 = f2.actor_ref.ask(GetMode).await.expect("GetMode f2");

        assert_eq!(mode1, AgentMode::Plan);
        assert_eq!(
            mode2,
            AgentMode::Build,
            "f2 should not be affected by f1's mode change"
        );
    }

    #[tokio::test]
    async fn duplicate_client_input_id_returns_original_receipt() {
        let f = ActorFixture::new().await;
        let input = SubmitInput {
            session_id: f._session_id.clone(),
            client_input_id: Some("client-dup-1".to_string()),
            expected_run_id: None,
            delivery: InputDelivery::Queue,
            // Must be a valid prompt: submissions are validated before the
            // receipt cache is consulted, so an empty prompt would be rejected
            // with InvalidPromptContent and never yield a replayable receipt.
            prompt: vec![crate::acp::protocol::ContentBlock::Text(
                crate::acp::protocol::TextContent::new("keyed submission"),
            )],
        };

        let first = f
            .actor_ref
            .ask(SubmitSessionInput::new(input.clone()))
            .await
            .expect("first keyed submission should be acknowledged");
        let second = f
            .actor_ref
            .ask(SubmitSessionInput::new(input))
            .await
            .expect("duplicate keyed submission should return the cached receipt");

        let first_receipt = match first {
            SubmitInputResult::Started { run_id, input_id } => (run_id, input_id),
            other => panic!("expected Started receipt, got {other:?}"),
        };
        assert_eq!(first_receipt.1, "client-dup-1");
        match second {
            SubmitInputResult::Started { run_id, input_id } => {
                assert_eq!(input_id, "client-dup-1");
                assert_eq!(
                    run_id, first_receipt.0,
                    "a repeated client_input_id must not start a second run"
                );
            }
            other => panic!("expected cached Started receipt, got {other:?}"),
        }
    }

    // ── Token state machine tests ─────────────────────────────────────────────

    #[test]
    fn client_prompt_id_uses_querymt_metadata_namespace() {
        let meta = serde_json::Map::from_iter([(
            "querymt".to_string(),
            serde_json::json!({"client_prompt_id": "client-123"}),
        )]);
        assert_eq!(
            client_prompt_id_from_meta(Some(&meta)).as_deref(),
            Some("client-123")
        );
        assert!(client_prompt_id_from_meta(None).is_none());
    }

    #[test]
    fn parse_transport_model_id_splits_on_first_slash() {
        let (provider, model) = parse_transport_model_id(
            "llama_cpp/hf:unsloth/Qwen3.5-4B-GGUF:Q5_K_M.gguf",
            "anthropic",
        );
        assert_eq!(provider, "llama_cpp");
        assert_eq!(model, "hf:unsloth/Qwen3.5-4B-GGUF:Q5_K_M.gguf");

        let (provider, model) =
            parse_transport_model_id("openrouter/openai/gpt-5.3-codex", "anthropic");
        assert_eq!(provider, "openrouter");
        assert_eq!(model, "openai/gpt-5.3-codex");
    }

    #[test]
    fn parse_transport_model_id_uses_fallback_for_legacy_bare_ids() {
        let (provider, model) = parse_transport_model_id("claude-opus-4-6", "anthropic");
        assert_eq!(provider, "anthropic");
        assert_eq!(model, "claude-opus-4-6");
    }

    #[tokio::test]
    async fn stale_prompt_finished_leaves_the_running_run_in_place() {
        let fixture = ActorFixture::new().await;
        let mut actor = SessionActor::new(
            fixture.config.clone(),
            fixture._session_id.clone(),
            crate::agent::core::SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            ),
        );
        let run_id = querymt_agent_core::model::TurnId::new();
        let running_token = CancellationToken::new();
        actor.generation = 4;
        actor.run = RunSlot::Running(
            ActiveRun::new(run_id, 4, running_token.clone()),
            TurnAuthority::Local,
        );
        let actor_ref = SessionActor::spawn(actor);

        // A finish of an older generation, even for the same run id.
        actor_ref
            .tell(PromptFinished {
                generation: 3,
                run_id,
                disposition: RunCompletionDisposition::Completed,
            })
            .await
            .expect("tell PromptFinished");
        let status = actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert_eq!(
            status.active_run_id.as_deref(),
            Some(run_id.to_string().as_str())
        );

        // Cancel still reaches the token of the run that is running.
        actor_ref.tell(Cancel).await.expect("tell Cancel");
        let status = actor_ref.ask(GetRuntimeStatus).await.unwrap();
        assert!(running_token.is_cancelled());
        assert_eq!(status.phase, SessionRuntimePhase::CancelRequested);
    }

    /// Simulates the cancel-then-resume scenario:
    ///
    /// 1. Prompt starts       → prompt_running=true, token shared with running task
    /// 2. Cancel arrives      → token cancelled (reaches the running task)
    /// 3. New prompt arrives  → token.is_cancelled() → fresh token created for new turn
    /// 4. Old task finishes   → PromptFinished resets prompt_running + token
    ///
    /// The new prompt's detached task waits on the execution_permit semaphore
    /// (held by the old task) so no two prompts run concurrently.
    #[test]
    fn cancel_flag_allows_new_prompt_while_old_task_winds_down() {
        let mut cancel_token = CancellationToken::new();

        // Step 1: Prompt starts — give the running task a clone of the token.
        let mut prompt_running = true;
        let _running_task_token = cancel_token.clone(); // simulates the spawned task's handle
        assert!(prompt_running);
        assert!(!cancel_token.is_cancelled());

        // Step 2: Cancel arrives — cancels the shared token, which the running task holds.
        cancel_token.cancel();
        assert!(cancel_token.is_cancelled());
        assert!(
            _running_task_token.is_cancelled(),
            "Cancel must reach the running task's token clone"
        );
        // prompt_running is still true because PromptFinished hasn't arrived yet.

        // Step 3: New prompt arrives while old task winds down.
        // The Prompt handler creates a fresh token only because the previous one is cancelled.
        let allow_new_prompt = if prompt_running {
            cancel_token.is_cancelled() // cancelled → allow new prompt
        } else {
            true
        };
        assert!(
            allow_new_prompt,
            "A cancelled session must accept new prompts even before PromptFinished arrives"
        );

        if cancel_token.is_cancelled() {
            cancel_token = CancellationToken::new();
        }
        assert!(
            !cancel_token.is_cancelled(),
            "New prompt's cancel_token should start clean"
        );

        // Step 4: PromptFinished arrives — resets prompt_running.
        // In the real actor the token is also reset here, but for this
        // scenario it was already reset in Step 3 (cancelled path).
        prompt_running = false;
        assert!(!prompt_running);
        assert!(!cancel_token.is_cancelled());
    }

    /// Verifies the non-cancelled path: a second Prompt arriving while the first
    /// task is still running must NOT replace the cancel token, so Cancel still
    /// reaches the first task.
    #[test]
    fn cancel_reaches_running_task_when_second_prompt_queued() {
        let mut cancel_token = CancellationToken::new();

        // Step 1: First prompt starts.
        let running_task_token = cancel_token.clone();
        assert!(!cancel_token.is_cancelled());

        // Step 2: Second prompt arrives while first is still running.
        // Because the token is NOT cancelled, the Prompt handler keeps the existing token.
        let should_replace = cancel_token.is_cancelled();
        assert!(
            !should_replace,
            "Must not replace token while task is alive"
        );
        // token unchanged — running task still holds the same token.

        // Step 3: Cancel arrives.
        cancel_token.cancel();
        assert!(
            running_task_token.is_cancelled(),
            "Cancel must reach the first task even after a second Prompt was queued"
        );

        // Step 4: PromptFinished — reset token for the next clean turn.
        cancel_token = CancellationToken::new();
        assert!(
            !cancel_token.is_cancelled(),
            "Token reset after PromptFinished"
        );
    }

    #[test]
    fn summarize_intent_candidate_ignores_empty_and_attachment_placeholder() {
        assert_eq!(summarize_intent_candidate("   \n  "), None);
        assert_eq!(
            summarize_intent_candidate(ATTACHMENTS_PLACEHOLDER),
            None,
            "attachment-only prompts should not become intent"
        );
    }

    #[test]
    fn summarize_intent_candidate_ukrainian_no_panic() {
        // Regression test for issue #525: byte 237 landed inside 'ч' (2-byte char).
        let ukrainian = "зроби коміт без gpg, до цього стану та почни виправленя. \
                         А ще, зараз проблема з подвійним натисканням шифт + клік, \
                         щоб переместити річ у сховище";
        let result = summarize_intent_candidate(ukrainian).unwrap();
        assert!(result.len() <= 240);
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn summarize_intent_candidate_cjk_no_panic() {
        let cjk = "你好".repeat(200); // 2 chars * 200 = 400 chars, each 3 bytes = 1200 bytes
        let result = summarize_intent_candidate(&cjk).unwrap();
        assert!(result.len() <= 240);
        assert!(result.is_char_boundary(result.len()));
    }

    // ── Model selection keeps the profile's parameters ───────────────────

    /// Provider factory that records every configuration it builds a provider from.
    struct CapturingFactory {
        provider: SharedLlmProvider,
        captured: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    impl querymt::plugin::LLMProviderFactory for CapturingFactory {
        fn name(&self) -> &str {
            "capture"
        }

        fn config_schema(&self) -> String {
            r#"{"properties":{"model":{},"max_tokens":{},"enable_thinking":{},"n_ctx":{}}}"#.into()
        }

        fn from_config(&self, cfg: &str) -> Result<Box<dyn querymt::LLMProvider>, LLMError> {
            let value = serde_json::from_str(cfg)?;
            self.captured.lock().expect("captured lock").push(value);
            Ok(Box::new(self.provider.clone()))
        }

        fn list_models<'a>(
            &'a self,
            _cfg: &str,
        ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn profile_params() -> LLMParams {
        LLMParams::new()
            .provider("capture")
            .model("M")
            .max_tokens(16_384)
            .parameter("enable_thinking", false)
            .parameter("n_ctx", 32_768)
    }

    /// Session actor over real storage, so config deduplication is exercised.
    struct SelectionFixture {
        config: Arc<AgentConfig>,
        actor_ref: kameo::actor::ActorRef<SessionActor>,
        session_id: String,
        captured: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    impl SelectionFixture {
        async fn new() -> Self {
            Self::build(|actor| actor).await
        }

        async fn build(customize: impl FnOnce(SessionActor) -> SessionActor) -> Self {
            let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
            let registry = querymt::plugin::host::PluginRegistry::empty();
            registry.register_static(Arc::new(CapturingFactory {
                provider: SharedLlmProvider {
                    inner: Arc::new(Mutex::new(MockLlmProvider::new())),
                    tools: vec![].into_boxed_slice(),
                },
                captured: captured.clone(),
            }));
            let storage = Arc::new(
                crate::session::sqlite_storage::SqliteStorage::connect(":memory:".into())
                    .await
                    .expect("create storage"),
            );
            let config = Arc::new(
                AgentConfigBuilder::new(Arc::new(registry), storage.clone(), profile_params())
                    .with_tool_policy(ToolPolicy::ProviderOnly)
                    .build(),
            );
            let session = storage
                .session_store()
                .create_session(None, None, None, None)
                .await
                .expect("create session");
            let runtime = SessionRuntime::new(
                None,
                HashMap::new(),
                crate::agent::core::McpToolState::empty(),
            );
            let actor = customize(SessionActor::new(
                config.clone(),
                session.public_id.clone(),
                runtime,
            ));
            Self {
                config,
                actor_ref: SessionActor::spawn(actor),
                session_id: session.public_id,
                captured,
            }
        }

        async fn select_on(
            &self,
            model_id: &str,
            provider_node_id: Option<&str>,
        ) -> SessionControlTransition {
            self.actor_ref
                .ask(SetSessionModel {
                    selection: crate::agent::session_control::SessionModelSelection {
                        model_id: model_id.to_string(),
                        provider_node_id: provider_node_id.map(str::to_string),
                    },
                })
                .await
                .expect("select model")
        }

        async fn select(&self, model_id: &str) -> SessionControlTransition {
            self.select_on(model_id, None).await
        }

        async fn control(&self) -> SessionControlState {
            self.actor_ref
                .ask(GetSessionControl)
                .await
                .expect("session control")
        }

        /// Stored parameters of the session's currently bound configuration.
        async fn stored_params(&self) -> LLMParams {
            let id = self.control().await.effective_model.llm_config_id;
            let row = self
                .config
                .provider
                .history_store()
                .get_llm_config(id)
                .await
                .expect("load config")
                .expect("config row");
            row.params
                .map(|value| serde_json::from_value(value).expect("parse params"))
                .unwrap_or_default()
        }
    }

    fn assert_profile_params(params: &LLMParams) {
        assert_eq!(params.max_tokens, Some(16_384));
        let custom = params.custom.as_ref().expect("custom parameters");
        assert_eq!(custom["enable_thinking"], serde_json::json!(false));
        assert_eq!(custom["n_ctx"], serde_json::json!(32_768));
    }

    fn drain_provider_changes(
        rx: &mut tokio::sync::broadcast::Receiver<querymt_agent_core::events::EventEnvelope>,
    ) -> usize {
        let mut changes = 0;
        while let Ok(envelope) = rx.try_recv() {
            if matches!(envelope.kind(), AgentEventKind::ProviderChanged { .. }) {
                changes += 1;
            }
        }
        changes
    }

    #[tokio::test]
    async fn selecting_profile_model_restores_profile_parameters_and_first_use_config() {
        let f = SelectionFixture::new().await;
        let first_use = f.control().await.effective_model.llm_config_id;

        let away = f.select("capture/N").await;
        assert_ne!(away.current.effective_model.llm_config_id, first_use);
        let other = f.stored_params().await;
        assert_eq!(other.max_tokens, None);
        assert!(
            other.custom.is_none_or(|custom| custom.is_empty()),
            "other models inherit no profile keys"
        );

        let back = f.select("capture/M").await;
        assert_eq!(back.current.effective_model.llm_config_id, first_use);
        assert_profile_params(&f.stored_params().await);
    }

    /// A sessions-only peer backed by a real grant store, with its grant state.
    #[cfg(feature = "remote")]
    async fn sessions_peer(
        dir: &tempfile::TempDir,
    ) -> (
        Arc<querymt_remote::MeshAccessStore>,
        PeerGrantKey,
        PeerGrantState,
        Arc<PeerTurn>,
    ) {
        let grants = Arc::new(querymt_remote::MeshAccessStore::new(
            querymt_remote::MeshStateStore::shared(&dir.path().join("mesh_state.json")).unwrap(),
        ));
        let key = PeerGrantKey {
            host: NodeId::from_peer_id(libp2p::PeerId::random()),
            peer: NodeId::from_peer_id(libp2p::PeerId::random()),
        };
        let grant = PeerGrant::Custom {
            sessions: true,
            filesystem: false,
            shell: false,
            providers: vec![],
        };
        let state = grants
            .replace_grant(&key, Uuid::nil(), grant.clone())
            .await
            .unwrap();
        let peer = PeerTurn::test_peer(key.clone(), grant, grants.clone());
        (grants, key, state, peer)
    }

    #[cfg(feature = "remote")]
    fn running(authority: TurnAuthority) -> impl FnOnce(SessionActor) -> SessionActor {
        move |mut actor| {
            actor.run = RunSlot::Running(
                ActiveRun::new(
                    querymt_agent_core::model::TurnId::new(),
                    1,
                    CancellationToken::new(),
                ),
                authority,
            );
            actor
        }
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn peer_shutdown_refuses_a_run_the_peer_cannot_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (_grants, _, _, peer) = sessions_peer(&dir).await;
        let f = SelectionFixture::build(running(TurnAuthority::Local)).await;

        let stopped = f.actor_ref.ask(PeerShutdown(peer)).await;
        assert!(matches!(
            stopped,
            Err(kameo::error::SendError::HandlerError(
                AgentError::MeshAccessDenied
            ))
        ));
        assert!(f.actor_ref.is_alive(), "the owner's run keeps its session");
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn peer_shutdown_stops_the_peers_own_run() {
        let dir = tempfile::tempdir().unwrap();
        let (_grants, _, _, peer) = sessions_peer(&dir).await;
        let f = SelectionFixture::build(running(TurnAuthority::Peer(peer.clone()))).await;

        f.actor_ref.ask(PeerShutdown(peer)).await.expect("stop");
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.actor_ref.wait_for_shutdown(),
        )
        .await
        .expect("actor stops");
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn peer_shutdown_rechecks_the_grant_before_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let (grants, key, state, peer) = sessions_peer(&dir).await;
        let f = SelectionFixture::new().await;
        grants
            .replace_grant(&key, state.revision, PeerGrant::None)
            .await
            .unwrap();

        let stopped = f.actor_ref.ask(PeerShutdown(peer)).await;
        assert!(matches!(
            stopped,
            Err(kameo::error::SendError::HandlerError(
                AgentError::MeshAccessDenied
            ))
        ));
        assert!(f.actor_ref.is_alive());
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn peer_llm_config_hides_provider_parameters_below_full_grant() {
        let f = SelectionFixture::new().await;
        f.control().await;
        let full = f
            .actor_ref
            .ask(GetPeerLlmConfig { full: true })
            .await
            .unwrap()
            .expect("bound config");
        assert!(full.params.is_some(), "fixture profile stores parameters");

        let restricted = f
            .actor_ref
            .ask(GetPeerLlmConfig { full: false })
            .await
            .unwrap()
            .expect("bound config");
        assert_eq!(
            (restricted.provider.as_str(), restricted.model.as_str()),
            (full.provider.as_str(), full.model.as_str())
        );
        assert!(restricted.params.is_none());
        assert!(restricted.endpoint.is_empty());
    }

    #[tokio::test]
    async fn set_provider_message_uses_selection_semantics() {
        let f = SelectionFixture::new().await;
        let first_use = f.control().await.effective_model.llm_config_id;
        f.select("capture/N").await;

        f.actor_ref
            .ask(SetProvider {
                provider: "capture".into(),
                model: "M".into(),
            })
            .await
            .expect("set provider");

        let control = f.control().await;
        assert_eq!(control.effective_model.llm_config_id, first_use);
        assert_profile_params(&f.stored_params().await);
    }

    #[tokio::test]
    async fn reselecting_current_binding_is_noop_for_every_local_node_spelling() {
        let f = SelectionFixture::new().await;
        let before = f.control().await;
        let mut rx = f.config.event_sink.fanout().subscribe();

        for node in [None, Some("local")] {
            let transition = f.select_on("capture/M", node).await;
            assert_eq!(transition.previous, before);
            assert_eq!(transition.current, before);
        }

        assert_eq!(f.control().await, before, "binding and revision unchanged");
        assert_eq!(drain_provider_changes(&mut rx), 0);
    }

    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn reselecting_current_binding_is_noop_for_own_peer_id() {
        let mesh = crate::agent::remote::test_helpers::fixtures::get_test_mesh().await;
        let own_id = mesh.peer_id().to_string();
        let f = SelectionFixture::build(|actor| actor.with_mesh(Some(mesh.clone()))).await;
        let before = f.control().await;
        let mut rx = f.config.event_sink.fanout().subscribe();

        let transition = f.select_on("capture/M", Some(&own_id)).await;

        assert_eq!(transition.current, before);
        assert_eq!(drain_provider_changes(&mut rx), 0);
    }

    #[tokio::test]
    async fn explicit_llm_config_survives_reselecting_the_same_model() {
        let f = SelectionFixture::new().await;
        f.actor_ref
            .ask(SetLlmConfig {
                config: LLMParams::new()
                    .provider("capture")
                    .model("M")
                    .max_tokens(4096),
                provider_node_id: None,
            })
            .await
            .expect("set llm config");
        let before = f.control().await;

        let transition = f.select("capture/M").await;

        assert_eq!(transition.current, before);
        let params = f.stored_params().await;
        assert_eq!(params.max_tokens, Some(4096));
        assert!(
            params.custom.is_none_or(|custom| custom.is_empty()),
            "the profile is not merged in"
        );
    }

    #[tokio::test]
    async fn provider_is_built_with_profile_parameters_after_model_selection() {
        let f = SelectionFixture::new().await;
        f.select("capture/N").await;
        f.select("capture/M").await;

        f.config
            .provider
            .build_provider_for_session(&f.session_id)
            .await
            .expect("build provider");

        let captured = f.captured.lock().expect("captured lock");
        let built = captured.last().expect("provider was built");
        assert_eq!(built["model"], "M");
        assert_eq!(built["max_tokens"], 16_384);
        assert_eq!(built["enable_thinking"], false);
        assert_eq!(built["n_ctx"], 32_768);
    }

    // ── one run id (session-run-lifecycle) ──────────────────────────────

    /// A command hook for `event` that appends its stdin (one JSON object per
    /// line) to `capture` and answers with an empty JSON object.
    fn capturing_hooks(events: &[&str], capture: &std::path::Path) -> crate::hooks::Hooks {
        let command = format!(
            "cat >> '{path}'; printf '\\n' >> '{path}'; printf '%s' '{{}}'",
            path = capture.display()
        );
        let mut config = serde_json::json!({"enabled": true});
        for event in events {
            config[event] = serde_json::json!([{"hooks": [{
                "type": "command", "command": command, "timeout_sec": 5
            }]}]);
        }
        crate::hooks::Hooks::new(serde_json::from_value(config).unwrap()).unwrap()
    }

    fn captured_inputs(capture: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(capture)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("hook input is JSON"))
            .collect()
    }

    fn hook_turn_ids(inputs: &[serde_json::Value], event: &str) -> Vec<String> {
        inputs
            .iter()
            .filter(|input| input["hook_event_name"] == event)
            .map(|input| input["turn_id"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn text_provider() -> crate::test_utils::SharedLlmProvider {
        use crate::test_utils::{MockChatResponse, MockLlmProvider, SharedLlmProvider};
        let mut mock = MockLlmProvider::new();
        mock.expect_chat_with_tools()
            .returning(|_, _| Ok(MockChatResponse::text_only("done").into()));
        mock.expect_chat()
            .returning(|_| Ok(MockChatResponse::text_only("done").into()));
        SharedLlmProvider::new(mock, vec![])
    }

    /// `(run_started run_id, run_completed run_id)` of the session's journal.
    async fn journal_run_ids(
        fixture: &crate::test_utils::UndoTestFixture,
        session: &str,
    ) -> (Vec<String>, Vec<String>) {
        fixture
            .handle
            .config
            .event_sink
            .session_stream(session)
            .flush()
            .await;
        let (mut started, mut completed) = (Vec::new(), Vec::new());
        for event in fixture
            .storage
            .event_journal()
            .load_session_stream(session, None, None)
            .await
            .unwrap()
        {
            match event.kind {
                AgentEventKind::RunStarted { run_id, .. } => started.push(run_id),
                AgentEventKind::RunCompleted { run_id, .. } => completed.push(run_id),
                _ => {}
            }
        }
        (started, completed)
    }

    #[tokio::test]
    async fn run_events_hooks_and_snapshot_records_share_the_run_id() {
        use crate::agent::undo_integration_tests::{bind_mock_llm, deleting_provider};
        let capture_dir = tempfile::tempdir().unwrap();
        let capture = capture_dir.path().join("hooks.jsonl");
        let fixture = crate::test_utils::UndoTestFixture::with_provider_and_hooks(
            deleting_provider("notes.txt"),
            capturing_hooks(&["pre_tool_use", "post_tool_use"], &capture),
        )
        .await
        .unwrap();
        fixture.write_file("notes.txt", "original").unwrap();
        let session = fixture.create_session().await.unwrap();
        bind_mock_llm(&fixture, &session).await.unwrap();

        fixture
            .prompt_to_completion(&session, "delete the notes")
            .await
            .unwrap();

        let (started, completed) = journal_run_ids(&fixture, &session).await;
        assert_eq!((started.len(), completed.len()), (1, 1));
        let run_id = started[0].clone();
        assert_eq!(completed[0], run_id, "run_completed run_id");
        assert!(uuid::Uuid::parse_str(&run_id).is_ok(), "run id is a UUID");

        // Both hooks of one tool call carry the run's id.
        let inputs = captured_inputs(&capture);
        assert_eq!(
            hook_turn_ids(&inputs, "pre_tool_use"),
            vec![run_id.clone()],
            "pre-tool-use hook turn_id"
        );
        assert_eq!(
            hook_turn_ids(&inputs, "post_tool_use"),
            vec![run_id.clone()],
            "post-tool-use hook turn_id"
        );

        let history = fixture
            .storage
            .session_store()
            .get_history(&session)
            .await
            .unwrap();
        let (mut starts, mut patches) = (Vec::new(), Vec::new());
        for part in history.iter().flat_map(|message| message.parts.iter()) {
            match part {
                MessagePart::TurnSnapshotStart { turn_id, .. } => starts.push(turn_id.to_string()),
                MessagePart::TurnSnapshotPatch { turn_id, .. } => patches.push(turn_id.to_string()),
                _ => {}
            }
        }
        assert_eq!(starts, vec![run_id.clone()], "snapshot start turn_id");
        assert_eq!(patches, vec![run_id], "snapshot change turn_id");
    }

    #[tokio::test]
    async fn steering_reports_the_active_runs_id() {
        let f = ActorFixture::new().await;
        // Hold the run open so the steering input reaches an active run.
        let _execution = f.execution_permit.acquire().await.unwrap();
        let SubmitInputResult::Started { run_id, .. } = f
            .actor_ref
            .ask(text_submit(&f._session_id, "hi"))
            .await
            .expect("submit")
        else {
            panic!("expected Started");
        };
        let mut steer = text_submit(&f._session_id, "adjust");
        steer.input.delivery = InputDelivery::Steer;
        steer.input.expected_run_id = Some(run_id.clone());
        let SubmitInputResult::Steered {
            run_id: steered, ..
        } = f.actor_ref.ask(steer).await.expect("steer")
        else {
            panic!("expected Steered");
        };
        assert_eq!(steered, run_id, "steering result run_id");

        f.config
            .event_sink
            .session_stream(&f._session_id)
            .flush()
            .await;
        let events = f.run_lifecycle().await;
        assert_eq!(events[0].1, steered, "run_started run_id");
        f.execution_permit.close();
    }

    #[tokio::test]
    async fn hook_fired_inside_a_run_gets_the_runs_id() {
        let capture_dir = tempfile::tempdir().unwrap();
        let capture = capture_dir.path().join("hooks.jsonl");
        let fixture = crate::test_utils::UndoTestFixture::with_provider_and_hooks(
            text_provider(),
            capturing_hooks(&["stop"], &capture),
        )
        .await
        .unwrap();
        let session = fixture.create_session().await.unwrap();
        crate::agent::undo_integration_tests::bind_mock_llm(&fixture, &session)
            .await
            .unwrap();

        fixture
            .prompt_to_completion(&session, "hello")
            .await
            .unwrap();

        let (started, _) = journal_run_ids(&fixture, &session).await;
        assert_eq!(started.len(), 1);
        let stop_ids = hook_turn_ids(&captured_inputs(&capture), "stop");
        assert_eq!(stop_ids, vec![started[0].clone()], "stop hook turn_id");
    }

    #[tokio::test]
    async fn native_compact_hook_gets_that_prompts_run_id() {
        let capture_dir = tempfile::tempdir().unwrap();
        let capture = capture_dir.path().join("hooks.jsonl");
        let fixture = crate::test_utils::UndoTestFixture::with_provider_and_hooks(
            text_provider(),
            capturing_hooks(&["pre_compaction"], &capture),
        )
        .await
        .unwrap();
        let session = fixture.create_session().await.unwrap();
        crate::agent::undo_integration_tests::bind_mock_llm(&fixture, &session)
            .await
            .unwrap();
        // History must exist or `/compact` has nothing to compact.
        fixture
            .prompt_to_completion(&session, "hello")
            .await
            .unwrap();

        // Native `/compact` is a run of its own (no model turn), so its hook
        // carries that run's id rather than the earlier prompt's.
        fixture
            .prompt_to_completion(&session, "/compact")
            .await
            .unwrap();

        let (started, completed) = journal_run_ids(&fixture, &session).await;
        assert_eq!((started.len(), completed.len()), (2, 2), "{started:?}");
        let ids = hook_turn_ids(&captured_inputs(&capture), "pre_compaction");
        assert_eq!(ids, vec![started[1].clone()], "compaction hook turn_id");
        assert_ne!(ids[0], started[0], "not the earlier prompt's run id");
    }
}
