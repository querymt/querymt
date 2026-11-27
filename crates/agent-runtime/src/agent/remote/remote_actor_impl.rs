//! Remote actor trait implementations for `SessionActor`.
//!
//! This module provides the `RemoteActor` and `RemoteMessage` trait
//! implementations that allow `SessionActor` to participate in the kameo
//! mesh. Everything here is behind `#[cfg(feature = "remote")]`.
//!
//! ## Why manual impls instead of derive macros?
//!
//! `#[derive(RemoteActor)]` and `#[remote_message]` are proc macros that
//! expand to code referencing `kameo::remote::*`, which only exists when
//! `kameo/remote` is enabled. We can't conditionally apply derive macros
//! with `#[cfg]`, so we implement the traits manually behind a feature gate.
//!
//! The `linkme::distributed_slice` registrations that the macros normally
//! generate are also included here — they are required for kameo's remote
//! dispatch to find our actor and message handlers at runtime.
//!
//! ## Guarded routes
//!
//! A guarded mesh only dispatches messages installed on its
//! [`InboundRouter`]. Every route goes through [`route`], which converts the
//! wire message into the local message (attaching the authenticated peer
//! where the handler needs it), asks the actor with bounded timeouts and
//! reports a stopped actor as a typed transport failure.

use crate::agent::file_proxy::FileProxyError;
use crate::agent::messages;
use crate::agent::remote::event_forwarder::PeerSubscription;
use crate::agent::remote::event_relay::{EventRelayActor, RelayLagged, RelayedEvent};
use crate::agent::remote::remote_call::{CallTimeouts, RemoteCallError};
use crate::agent::session_actor::{
    PeerInteraction, PeerTurn, SessionActor, SubscribePeerEvents, TurnAuthority,
    UnsubscribePeerEvents,
};
use crate::agent::undo::UndoError;
use crate::error::AgentError;
use kameo::actor::{ActorRef, WeakActorRef};
use kameo::error::SendError;
use kameo::message::Message;
use kameo::remote::{_internal, RemoteActor, RemoteMessage};
use kameo::reply::Reply;
use querymt_remote::mesh_ingress::{InboundAccess, InboundDelivery, InboundRouter, PeerRequest};
use querymt_remote::peer_access::GrantStore;
use querymt_remote::{DeliveryCertainty, RemoteTransportFailure, RemoteTransportFailureKind};
use std::sync::Arc;
use std::time::Duration;

/// Session and node reads, and owner-wide controls.
pub(crate) const READ: CallTimeouts =
    CallTimeouts::new(Duration::from_secs(10), Duration::from_secs(60));
/// Node-manager mutations that may materialize a session (create, resume,
/// fork). The ingress dispatch budget, the owner's request timeout shortened
/// to the caller's reply timeout, is what bounds them; this cap must not
/// undercut it, or a slow materialization would fail after it committed.
pub(crate) const NODE_MUTATION: CallTimeouts =
    CallTimeouts::new(Duration::from_secs(10), Duration::from_secs(600));
/// Session controls answered from actor state.
const CONTROL: CallTimeouts = CallTimeouts::uniform(Duration::from_secs(10));
/// A turn runs until the model finishes.
const TURN: CallTimeouts = CallTimeouts::new(Duration::from_secs(10), Duration::from_secs(600));
/// Workspace reads and subscriptions must not queue behind a busy session.
const FAST_INDEX: CallTimeouts = CallTimeouts::new(Duration::from_secs(3), Duration::from_secs(10));
const FAST_READ: CallTimeouts = CallTimeouts::new(Duration::from_secs(3), Duration::from_secs(30));

/// The error a route returns on the wire for a local reply error: the type the
/// caller decodes, which is [`AgentError`] for infallible replies.
pub(crate) trait WireError: Sized {
    type Wire: RemoteCallError + From<Self> + serde::Serialize + Send + 'static;
}

macro_rules! wire_error {
    ($($error:ty => $wire:ty),+ $(,)?) => {
        $(impl WireError for $error {
            type Wire = $wire;
        })+
    };
}

wire_error!(
    AgentError => AgentError,
    kameo::error::Infallible => AgentError,
    UndoError => UndoError,
    FileProxyError => FileProxyError,
    agent_client_protocol::Error => agent_client_protocol::Error,
);

type ReplyOk<A, W> = <<A as Message<W>>::Reply as Reply>::Ok;
type ReplyWire<A, W> = <<<A as Message<W>>::Reply as Reply>::Error as WireError>::Wire;

/// Ask a local actor on behalf of a mesh peer.
///
/// # Errors
///
/// Returns the handler error, or an `ActorUnavailable`, `MailboxFull` or
/// `ReplyTimeout` transport failure with the delivery certainty kameo proves.
pub(crate) async fn ask_local<A, W>(
    actor: &WeakActorRef<A>,
    message: W,
    timeouts: CallTimeouts,
) -> Result<ReplyOk<A, W>, ReplyWire<A, W>>
where
    A: kameo::Actor + Message<W>,
    W: Send + 'static,
    <A::Reply as Reply>::Error: WireError,
{
    use DeliveryCertainty::{NotDelivered, Unknown};
    use RemoteTransportFailureKind as Kind;

    let actor = actor.upgrade().ok_or_else(|| {
        ReplyWire::<A, W>::transport(RemoteTransportFailure::new(
            Kind::ActorUnavailable,
            NotDelivered,
            "actor stopped",
        ))
    })?;
    let (kind, delivery, detail) = match actor
        .ask(message)
        .mailbox_timeout(timeouts.mailbox)
        .reply_timeout(timeouts.reply)
        .send()
        .await
    {
        Ok(reply) => return Ok(reply),
        Err(SendError::HandlerError(error)) => return Err(error.into()),
        Err(SendError::ActorNotRunning(_) | SendError::ActorRestarting(_)) => {
            (Kind::ActorUnavailable, NotDelivered, "actor not running")
        }
        Err(SendError::ActorStopped) => (Kind::ActorUnavailable, Unknown, "actor stopped"),
        Err(SendError::MailboxFull(_) | SendError::Timeout(Some(_))) => {
            (Kind::MailboxFull, NotDelivered, "actor mailbox full")
        }
        Err(SendError::Timeout(None)) => (Kind::ReplyTimeout, Unknown, "actor reply timed out"),
    };
    Err(ReplyWire::<A, W>::transport(RemoteTransportFailure::new(
        kind, delivery, detail,
    )))
}

/// Install the guarded route for wire message `M` on `actor`.
///
/// `local` turns the decoded wire message into the message the actor
/// handles. It runs before the ask, so it is where the authenticated peer is
/// attached; it must never copy authority from the wire payload.
pub(crate) fn route<A, M, W, F>(
    router: &InboundRouter,
    actor: &ActorRef<A>,
    delivery: InboundDelivery,
    access: InboundAccess,
    timeouts: CallTimeouts,
    local: F,
) where
    A: kameo::Actor + RemoteActor + RemoteMessage<M> + Message<W>,
    M: serde::de::DeserializeOwned + Send + 'static,
    W: Send + 'static,
    F: Fn(PeerRequest, M) -> Result<W, ReplyWire<A, W>> + Send + Sync + 'static,
    <A::Reply as Reply>::Ok: serde::Serialize + Send,
    <A::Reply as Reply>::Error: WireError,
{
    let weak = actor.downgrade();
    let handler = move |origin, message| {
        let weak = weak.clone();
        let message = local(origin, message);
        async move { ask_local(&weak, message?, timeouts).await }
    };
    router.register::<A, M, _, _, _, _>(actor.id(), delivery, access, handler);
}

/// Route a message the actor handles as received.
pub(crate) fn route_direct<A, M>(
    router: &InboundRouter,
    actor: &ActorRef<A>,
    access: InboundAccess,
    timeouts: CallTimeouts,
) where
    A: kameo::Actor + RemoteActor + RemoteMessage<M> + Message<M>,
    M: serde::de::DeserializeOwned + Send + 'static,
    <A::Reply as Reply>::Ok: serde::Serialize + Send,
    <A::Reply as Reply>::Error: WireError,
{
    route::<A, M, M, _>(
        router,
        actor,
        InboundDelivery::Ask,
        access,
        timeouts,
        |_, message| Ok(message),
    );
}

/// Use only for reads that are safe under the session grant. Mutations must
/// carry their authenticated origin into the actor, or require full access.
pub(crate) fn route_read<A, M>(router: &InboundRouter, actor: &ActorRef<A>)
where
    A: kameo::Actor + RemoteActor + RemoteMessage<M> + Message<M>,
    M: serde::de::DeserializeOwned + Send + 'static,
    <A::Reply as Reply>::Ok: serde::Serialize + Send,
    <A::Reply as Reply>::Error: WireError,
{
    route_direct::<A, M>(router, actor, InboundAccess::Sessions, READ);
}

pub(crate) fn register_session_history(router: &InboundRouter, actor: &ActorRef<SessionActor>) {
    route_read::<_, messages::GetHistory>(router, actor);
    route_read::<_, messages::GetUserPrompts>(router, actor);
    route_read::<_, messages::GetEventStream>(router, actor);
    route_read::<_, messages::GetEventStreamSince>(router, actor);
    route_read::<_, messages::GetEventStreamChunk>(router, actor);
}

pub(crate) fn register_session_routes(
    mesh: &querymt_remote::MeshHandle,
    actor: &ActorRef<SessionActor>,
) {
    let (Some(router), Some(store)) = (mesh.inbound_router(), mesh.quota_store()) else {
        return;
    };
    register_session_history(router, actor);
    register_session_controls(router, actor);
    register_file_proxy(router, actor);
    register_session_subscriptions(router, mesh, store.clone(), actor);
    register_session_turns(router, store.clone(), actor);
    register_session_interactions(router, store.clone(), actor);
}

/// Reads of session state, and controls that change the session's model,
/// tools context or workspace. Those controls are not bounded by a session
/// grant's model and tool limits, so they need full access.
fn register_session_controls(router: &InboundRouter, actor: &ActorRef<SessionActor>) {
    route_read::<_, messages::SessionDeliveryBarrier>(router, actor);
    route_read::<_, messages::GetSessionControl>(router, actor);
    route_read::<_, messages::GetMode>(router, actor);
    route_read::<_, messages::GetSessionLimits>(router, actor);
    route::<_, messages::GetLlmConfig, _, _>(
        router,
        actor,
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        READ,
        |origin, _| {
            Ok(crate::agent::session_actor::GetPeerLlmConfig {
                full: matches!(
                    origin.grant(),
                    Some(querymt_remote::peer_access::PeerGrant::Full)
                ),
            })
        },
    );
    route_read::<_, messages::GetReasoningEffort>(router, actor);
    macro_rules! full {
        ($($message:ty),+ $(,)?) => {
            $(route_direct::<_, $message>(router, actor, InboundAccess::Full, READ);)+
        };
    }
    full!(
        messages::SetMode,
        messages::SetReasoningEffort,
        messages::SetLlmConfig,
        messages::SetPlanningContext,
        messages::Undo,
        messages::Redo,
    );
    // Selecting a model on another node spends nothing on this host: that node
    // authorizes and meters the calls, under the caller's own node (a model on
    // the caller's node), or under the caller's model pass or, for Full
    // access, this host's access (a third node). The binding alone confers no
    // use of the model, so the session grant is enough. Selecting one of this
    // host's own models still needs full access.
    route::<_, messages::SetSessionModel, _, _>(
        router,
        actor,
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        READ,
        |origin, message: messages::SetSessionModel| {
            let full = matches!(
                origin.grant(),
                Some(querymt_remote::peer_access::PeerGrant::Full)
            );
            if full || selects_remote_node(&origin, &message.selection) {
                Ok(message)
            } else {
                tracing::warn!(
                    peer = %origin.key().peer,
                    model = %message.selection.model_id,
                    provider_node = message.selection.provider_node_id.as_deref().unwrap_or(""),
                    "mesh model selection denied: only full access may select a model on this host"
                );
                Err(AgentError::MeshAccessDenied)
            }
        },
    );
}

/// Whether `selection` pins the session's model to a node other than this host.
fn selects_remote_node(
    origin: &PeerRequest,
    selection: &crate::agent::session_control::SessionModelSelection,
) -> bool {
    selection
        .provider_node_id
        .as_deref()
        .and_then(|node| querymt_remote::NodeId::parse(node).ok())
        .is_some_and(|node| node != origin.key().host)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "callers pass `Arc<dyn QuotaStore>`; by-value is what upcasts it to `Arc<dyn GrantStore>`"
)]
fn register_session_interactions(
    router: &InboundRouter,
    store: Arc<dyn GrantStore>,
    actor: &ActorRef<SessionActor>,
) {
    fn interaction<M>(
        router: &InboundRouter,
        store: &Arc<dyn GrantStore>,
        actor: &ActorRef<SessionActor>,
    ) where
        SessionActor: RemoteMessage<M> + Message<PeerInteraction<M>>,
        <SessionActor as Message<PeerInteraction<M>>>::Reply:
            Reply<Error = AgentError, Ok: serde::Serialize + Send>,
        M: serde::de::DeserializeOwned + Send + 'static,
    {
        let store = store.clone();
        route::<SessionActor, M, _, _>(
            router,
            actor,
            InboundDelivery::Ask,
            InboundAccess::Sessions,
            CONTROL,
            move |origin, message| {
                let peer = Arc::new(PeerTurn::from_request(&origin, store.clone())?);
                Ok(PeerInteraction { peer, message })
            },
        );
    }
    interaction::<messages::ListPendingInteractions>(router, &store, actor);
    interaction::<messages::ClaimInteractionDelivery>(router, &store, actor);
    interaction::<messages::ResolveInteraction>(router, &store, actor);
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "callers pass `Arc<dyn QuotaStore>`; by-value is what upcasts it to `Arc<dyn GrantStore>`"
)]
pub(super) fn register_session_turns(
    router: &InboundRouter,
    store: Arc<dyn GrantStore>,
    actor: &ActorRef<SessionActor>,
) {
    fn turn<M>(
        router: &InboundRouter,
        store: &Arc<dyn GrantStore>,
        actor: &ActorRef<SessionActor>,
        timeouts: CallTimeouts,
        authority: fn(&mut M) -> &mut TurnAuthority,
        pass: fn(&mut M) -> Option<querymt_remote::ModelPass>,
    ) where
        SessionActor: RemoteMessage<M> + Message<M>,
        <SessionActor as Message<M>>::Reply: Reply<Ok: serde::Serialize + Send>,
        <<SessionActor as Message<M>>::Reply as Reply>::Error: WireError<Wire = AgentError>,
        M: serde::de::DeserializeOwned + Send + 'static,
    {
        let store = store.clone();
        route::<SessionActor, M, M, _>(
            router,
            actor,
            InboundDelivery::Ask,
            InboundAccess::Sessions,
            timeouts,
            move |origin, mut message| {
                let pass = pass(&mut message);
                *authority(&mut message) = TurnAuthority::Peer(Arc::new(
                    PeerTurn::from_request(&origin, store.clone())?.with_pass(pass),
                ));
                Ok(message)
            },
        );
    }
    turn::<messages::Prompt>(
        router,
        &store,
        actor,
        TURN,
        |msg| &mut msg.authority,
        |msg| msg.pass.take(),
    );
    turn::<messages::SubmitSessionInput>(
        router,
        &store,
        actor,
        CONTROL,
        |msg| &mut msg.authority,
        |msg| msg.pass.take(),
    );
    turn::<messages::DiscardQueuedInput>(
        router,
        &store,
        actor,
        CONTROL,
        |msg| &mut msg.authority,
        |_| None,
    );
    turn::<messages::GetRuntimeStatus>(
        router,
        &store,
        actor,
        CONTROL,
        |msg| &mut msg.authority,
        |_| None,
    );
    turn::<messages::Cancel>(
        router,
        &store,
        actor,
        CONTROL,
        |msg| &mut msg.authority,
        |_| None,
    );
}

pub(super) fn register_file_proxy(router: &InboundRouter, actor: &ActorRef<SessionActor>) {
    use crate::agent::session_actor::{GetPeerFileIndex, ReadPeerFile};

    route::<_, messages::GetFileIndex, _, _>(
        router,
        actor,
        InboundDelivery::Ask,
        InboundAccess::Filesystem,
        FAST_INDEX,
        |_, _| Ok(GetPeerFileIndex),
    );
    route::<_, messages::ReadRemoteFile, _, _>(
        router,
        actor,
        InboundDelivery::Ask,
        InboundAccess::Filesystem,
        FAST_READ,
        |_, message| Ok(ReadPeerFile(message)),
    );
}

pub(super) fn register_session_subscriptions(
    router: &InboundRouter,
    mesh: &querymt_remote::MeshHandle,
    store: Arc<dyn GrantStore>,
    actor: &ActorRef<SessionActor>,
) {
    let egress = mesh.sharing_egress();
    route::<_, messages::SubscribeEvents, _, _>(
        router,
        actor,
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        FAST_INDEX,
        move |origin, message| {
            Ok(SubscribePeerEvents {
                message,
                subscriber: PeerSubscription {
                    egress: egress.clone(),
                    store: store.clone(),
                    changes: origin.take_policy_changes(),
                    origin,
                },
            })
        },
    );
    route::<_, messages::UnsubscribeEvents, _, _>(
        router,
        actor,
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        FAST_INDEX,
        |origin, message| {
            Ok(UnsubscribePeerEvents {
                message,
                peer: *origin.key().peer.as_peer_id(),
            })
        },
    );
}

// ── RemoteActor ──────────────────────────────────────────────────────────

impl kameo::remote::RemoteActor for SessionActor {
    const REMOTE_ID: &'static str = "querymt::SessionActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static SESSION_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <SessionActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<SessionActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<SessionActor>(actor_id, sibling_id))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<SessionActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

// ── RemoteMessage implementations ────────────────────────────────────────
//
// Each message type needs:
// 1. `impl RemoteMessage<Msg> for SessionActor { const REMOTE_ID = "..."; }`
// 2. A `distributed_slice` entry registering ask/tell/try_ask/try_tell handlers

macro_rules! remote_msg_impl {
    ($msg_ty:ty, $remote_id:expr, $static_name:ident) => {
        impl kameo::remote::RemoteMessage<$msg_ty> for SessionActor {
            const REMOTE_ID: &'static str = $remote_id;
        }

        #[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
        #[linkme(crate = _internal::linkme)]
        static $static_name: (
            _internal::RemoteMessageRegistrationID<'static>,
            _internal::RemoteMessageFns,
        ) = (
            _internal::RemoteMessageRegistrationID {
                actor_remote_id: <SessionActor as kameo::remote::RemoteActor>::REMOTE_ID,
                message_remote_id:
                    <SessionActor as kameo::remote::RemoteMessage<$msg_ty>>::REMOTE_ID,
            },
            _internal::RemoteMessageFns {
                ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
                    Box::pin(_internal::ask::<SessionActor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                        reply_timeout,
                    ))
                }) as _internal::RemoteAskFn,
                try_ask: (|actor_id, msg, reply_timeout| {
                    Box::pin(_internal::try_ask::<SessionActor, $msg_ty>(
                        actor_id,
                        msg,
                        reply_timeout,
                    ))
                }) as _internal::RemoteTryAskFn,
                tell: (|actor_id, msg, mailbox_timeout| {
                    Box::pin(_internal::tell::<SessionActor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                    ))
                }) as _internal::RemoteTellFn,
                try_tell: (|actor_id, msg| {
                    Box::pin(_internal::try_tell::<SessionActor, $msg_ty>(actor_id, msg))
                }) as _internal::RemoteTryTellFn,
            },
        );
    };
}

// ── Register all remotely-accessible messages ────────────────────────────

remote_msg_impl!(messages::Prompt, "querymt::Prompt", REG_PROMPT);
remote_msg_impl!(
    messages::SubmitSessionInput,
    "querymt::SubmitSessionInput",
    REG_SUBMIT_SESSION_INPUT
);
remote_msg_impl!(
    messages::DiscardQueuedInput,
    "querymt::DiscardQueuedInput",
    REG_DISCARD_QUEUED_INPUT
);
remote_msg_impl!(messages::Cancel, "querymt::Cancel", REG_CANCEL);
remote_msg_impl!(messages::SetMode, "querymt::SetMode", REG_SET_MODE);
remote_msg_impl!(messages::GetMode, "querymt::GetMode", REG_GET_MODE);
remote_msg_impl!(
    messages::GetSessionControl,
    "querymt::GetSessionControl",
    REG_GET_SESSION_CONTROL
);
remote_msg_impl!(messages::Undo, "querymt::Undo", REG_UNDO);
remote_msg_impl!(messages::Redo, "querymt::Redo", REG_REDO);
remote_msg_impl!(
    messages::SetSessionModel,
    "querymt::SetSessionModel",
    REG_SET_SESSION_MODEL
);
remote_msg_impl!(messages::GetHistory, "querymt::GetHistory", REG_GET_HISTORY);
remote_msg_impl!(
    messages::GetUserPrompts,
    "querymt::GetUserPrompts",
    REG_GET_USER_PROMPTS
);
remote_msg_impl!(
    messages::GetEventStream,
    "querymt::GetEventStream",
    REG_GET_EVENT_STREAM
);
remote_msg_impl!(
    messages::GetEventStreamSince,
    "querymt::GetEventStreamSince",
    REG_GET_EVENT_STREAM_SINCE
);
remote_msg_impl!(
    messages::GetEventStreamChunk,
    "querymt::GetEventStreamChunk",
    REG_GET_EVENT_STREAM_CHUNK
);
remote_msg_impl!(
    messages::GetLlmConfig,
    "querymt::GetLlmConfig",
    REG_GET_LLM_CONFIG
);
remote_msg_impl!(
    messages::GetSessionLimits,
    "querymt::GetSessionLimits",
    REG_GET_SESSION_LIMITS
);
remote_msg_impl!(
    messages::GetRuntimeStatus,
    "querymt::GetRuntimeStatus",
    REG_GET_RUNTIME_STATUS
);
remote_msg_impl!(
    messages::SessionDeliveryBarrier,
    "querymt::SessionDeliveryBarrier",
    REG_SESSION_DELIVERY_BARRIER
);
remote_msg_impl!(
    messages::SetLlmConfig,
    "querymt::SetLlmConfig",
    REG_SET_LLM_CONFIG
);
remote_msg_impl!(
    messages::SubscribeEvents,
    "querymt::SubscribeEvents",
    REG_SUBSCRIBE_EVENTS
);
remote_msg_impl!(
    messages::UnsubscribeEvents,
    "querymt::UnsubscribeEvents",
    REG_UNSUBSCRIBE_EVENTS
);
remote_msg_impl!(
    messages::SetPlanningContext,
    "querymt::SetPlanningContext",
    REG_SET_PLANNING_CONTEXT
);
remote_msg_impl!(
    messages::GetFileIndex,
    "querymt::GetFileIndex",
    REG_GET_FILE_INDEX
);
remote_msg_impl!(
    messages::ReadRemoteFile,
    "querymt::ReadRemoteFile",
    REG_READ_REMOTE_FILE
);
remote_msg_impl!(
    messages::SetReasoningEffort,
    "querymt::SetReasoningEffort",
    REG_SET_REASONING_EFFORT
);
remote_msg_impl!(
    messages::GetReasoningEffort,
    "querymt::GetReasoningEffort",
    REG_GET_REASONING_EFFORT
);

remote_msg_impl!(
    messages::ListPendingInteractions,
    "querymt::ListPendingInteractions",
    REG_LIST_PENDING_INTERACTIONS
);
remote_msg_impl!(
    messages::ClaimInteractionDelivery,
    "querymt::ClaimInteractionDelivery",
    REG_CLAIM_INTERACTION_DELIVERY
);
remote_msg_impl!(
    messages::ResolveInteraction,
    "querymt::ResolveInteraction",
    REG_RESOLVE_INTERACTION
);

/// Accept callbacks only from the session owner selected at attachment time.
/// The destination is specific to one attachment and authorizes no other RPC.
pub(crate) fn register_session_relay(
    router: &InboundRouter,
    relay: &ActorRef<EventRelayActor>,
    owner: libp2p::PeerId,
    session_id: &str,
) {
    macro_rules! register_callback {
        ($message:ty, $session:expr) => {{
            let weak = relay.downgrade();
            let session_id = session_id.to_owned();
            let deliver = move |origin: querymt_remote::mesh_ingress::PeerRequest,
                                message: $message| {
                let weak = weak.clone();
                let matches_attachment =
                    origin.key().peer.as_peer_id() == &owner && ($session)(&message, &session_id);
                async move {
                    if !matches_attachment {
                        return Err(AgentError::MeshAccessDenied);
                    }
                    let relay = weak
                        .upgrade()
                        .ok_or_else(|| AgentError::RemoteActor("session relay stopped".into()))?;
                    relay
                        .tell(message)
                        .mailbox_timeout(Duration::from_secs(3))
                        .send()
                        .await
                        .map_err(|error| AgentError::RemoteActor(error.to_string()))
                }
            };
            router.register::<EventRelayActor, $message, _, _, _, _>(
                relay.id(),
                InboundDelivery::LegacyTellV1Ask,
                InboundAccess::Identity,
                deliver,
            );
        }};
    }
    register_callback!(RelayedEvent, |msg: &RelayedEvent, id: &str| msg
        .event
        .session_id
        == id);
    register_callback!(RelayLagged, |msg: &RelayLagged, id: &str| msg.session_id
        == id);
}

// ── EventRelayActor RemoteActor ──────────────────────────────────────────

impl kameo::remote::RemoteActor for EventRelayActor {
    const REMOTE_ID: &'static str = "querymt::EventRelayActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static EVENT_RELAY_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <EventRelayActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<EventRelayActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<EventRelayActor>(actor_id, sibling_id))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<EventRelayActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

// ── EventRelayActor RemoteMessage ────────────────────────────────────────

impl kameo::remote::RemoteMessage<RelayLagged> for EventRelayActor {
    const REMOTE_ID: &'static str = "querymt::RelayLagged";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
#[linkme(crate = _internal::linkme)]
static REG_RELAY_LAGGED: (
    _internal::RemoteMessageRegistrationID<'static>,
    _internal::RemoteMessageFns,
) = (
    _internal::RemoteMessageRegistrationID {
        actor_remote_id: <EventRelayActor as kameo::remote::RemoteActor>::REMOTE_ID,
        message_remote_id:
            <EventRelayActor as kameo::remote::RemoteMessage<RelayLagged>>::REMOTE_ID,
    },
    _internal::RemoteMessageFns {
        ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
            Box::pin(_internal::ask::<EventRelayActor, RelayLagged>(
                actor_id,
                msg,
                mailbox_timeout,
                reply_timeout,
            ))
        }) as _internal::RemoteAskFn,
        try_ask: (|actor_id, msg, reply_timeout| {
            Box::pin(_internal::try_ask::<EventRelayActor, RelayLagged>(
                actor_id,
                msg,
                reply_timeout,
            ))
        }) as _internal::RemoteTryAskFn,
        tell: (|actor_id, msg, mailbox_timeout| {
            Box::pin(_internal::tell::<EventRelayActor, RelayLagged>(
                actor_id,
                msg,
                mailbox_timeout,
            ))
        }) as _internal::RemoteTellFn,
        try_tell: (|actor_id, msg| {
            Box::pin(_internal::try_tell::<EventRelayActor, RelayLagged>(
                actor_id, msg,
            ))
        }) as _internal::RemoteTryTellFn,
    },
);

impl kameo::remote::RemoteMessage<RelayedEvent> for EventRelayActor {
    const REMOTE_ID: &'static str = "querymt::RelayedEvent";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
#[linkme(crate = _internal::linkme)]
static REG_RELAYED_EVENT: (
    _internal::RemoteMessageRegistrationID<'static>,
    _internal::RemoteMessageFns,
) = (
    _internal::RemoteMessageRegistrationID {
        actor_remote_id: <EventRelayActor as kameo::remote::RemoteActor>::REMOTE_ID,
        message_remote_id:
            <EventRelayActor as kameo::remote::RemoteMessage<RelayedEvent>>::REMOTE_ID,
    },
    _internal::RemoteMessageFns {
        ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
            Box::pin(_internal::ask::<EventRelayActor, RelayedEvent>(
                actor_id,
                msg,
                mailbox_timeout,
                reply_timeout,
            ))
        }) as _internal::RemoteAskFn,
        try_ask: (|actor_id, msg, reply_timeout| {
            Box::pin(_internal::try_ask::<EventRelayActor, RelayedEvent>(
                actor_id,
                msg,
                reply_timeout,
            ))
        }) as _internal::RemoteTryAskFn,
        tell: (|actor_id, msg, mailbox_timeout| {
            Box::pin(_internal::tell::<EventRelayActor, RelayedEvent>(
                actor_id,
                msg,
                mailbox_timeout,
            ))
        }) as _internal::RemoteTellFn,
        try_tell: (|actor_id, msg| {
            Box::pin(_internal::try_tell::<EventRelayActor, RelayedEvent>(
                actor_id, msg,
            ))
        }) as _internal::RemoteTryTellFn,
    },
);
