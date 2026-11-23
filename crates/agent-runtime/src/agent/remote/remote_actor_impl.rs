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

use crate::agent::messages;
use crate::agent::remote::event_forwarder::PeerSubscription;
use crate::agent::remote::event_relay::{EventRelayActor, RelayLagged, RelayedEvent};
use crate::agent::session_actor::{
    PeerTurn, SessionActor, SubscribePeerEvents, TurnAuthority, UnsubscribePeerEvents,
};
use crate::error::AgentError;
use kameo::actor::ActorRef;
use kameo::message::Message;
use kameo::remote::{_internal, RemoteActor, RemoteMessage};
use kameo::reply::Reply;
use querymt_remote::mesh_ingress::{InboundAccess, InboundDelivery, InboundRouter, PeerRequest};
use querymt_remote::peer_access::GrantStore;
use std::sync::Arc;
use std::time::Duration;

// Use only for reads that are safe under the session grant. Mutations must
// carry their authenticated origin into the actor and deferred work instead.
pub(crate) fn register_session_read<A, M>(router: &InboundRouter, actor: &ActorRef<A>)
where
    A: RemoteActor + RemoteMessage<M> + Message<M>,
    M: serde::de::DeserializeOwned + Send + 'static,
    <A::Reply as Reply>::Ok: serde::Serialize + Send,
    <A::Reply as Reply>::Error: std::fmt::Display,
    AgentError: From<<A::Reply as Reply>::Error>,
{
    register_session_request::<A, M>(router, actor, InboundAccess::Sessions);
}

pub(super) fn register_session_request<A, M>(
    router: &InboundRouter,
    actor: &ActorRef<A>,
    v1_access: InboundAccess,
) where
    A: RemoteActor + RemoteMessage<M> + Message<M>,
    M: serde::de::DeserializeOwned + Send + 'static,
    <A::Reply as Reply>::Ok: serde::Serialize + Send,
    <A::Reply as Reply>::Error: std::fmt::Display,
    AgentError: From<<A::Reply as Reply>::Error>,
{
    let weak = actor.downgrade();
    let read = move |_, msg| {
        let weak = weak.clone();
        async move {
            let actor = weak
                .upgrade()
                .ok_or_else(|| AgentError::RemoteActor("session reader stopped".into()))?;
            actor
                .ask(msg)
                .mailbox_timeout(Duration::from_secs(10))
                .reply_timeout(Duration::from_secs(60))
                .send()
                .await
                .map_err(|error| match error {
                    kameo::error::SendError::HandlerError(error) => error.into(),
                    other => AgentError::RemoteActor(other.to_string()),
                })
        }
    };
    router.register::<A, M, _, _, _, _>(actor.id(), InboundDelivery::Ask, v1_access, read);
}

pub(crate) fn register_session_history(router: &InboundRouter, actor: &ActorRef<SessionActor>) {
    register_session_read::<_, messages::GetHistory>(router, actor);
    register_session_read::<_, messages::GetUserPrompts>(router, actor);
    register_session_read::<_, messages::GetEventStream>(router, actor);
    register_session_read::<_, messages::GetEventStreamSince>(router, actor);
    register_session_read::<_, messages::GetEventStreamChunk>(router, actor);
}

pub(crate) fn register_session_routes(
    mesh: &querymt_remote::MeshHandle,
    actor: &ActorRef<SessionActor>,
) {
    let (Some(router), Some(store)) = (mesh.inbound_router(), mesh.quota_store()) else {
        return;
    };
    register_session_history(router, actor);
    register_session_read::<_, messages::SessionDeliveryBarrier>(router, actor);
    register_session_read::<_, messages::GetSessionControl>(router, actor);
    register_session_read::<_, messages::GetMode>(router, actor);
    register_session_request::<_, messages::SetMode>(router, actor, InboundAccess::Full);
    register_session_read::<_, messages::GetSessionLimits>(router, actor);
    register_file_proxy(router, actor);
    register_session_subscriptions(router, mesh, store.clone(), actor);
    register_session_turns(router, store.clone(), actor);
    register_session_interactions(router, store.clone(), actor);
}

fn register_session_interactions(
    router: &InboundRouter,
    store: Arc<dyn GrantStore>,
    actor: &ActorRef<SessionActor>,
) {
    register_peer_interaction::<messages::ListPendingInteractions, _>(router, &store, actor);
    register_peer_interaction::<messages::ClaimInteractionDelivery, _>(router, &store, actor);
    register_peer_interaction::<messages::ResolveInteraction, _>(router, &store, actor);
}

fn register_peer_interaction<M, R>(
    router: &InboundRouter,
    store: &Arc<dyn GrantStore>,
    actor: &ActorRef<SessionActor>,
) where
    SessionActor: RemoteMessage<M>
        + Message<crate::agent::session_actor::PeerInteraction<M>, Reply = Result<R, AgentError>>,
    M: serde::de::DeserializeOwned + Send + 'static,
    R: serde::Serialize + Send + 'static,
{
    let weak = actor.downgrade();
    let store = store.clone();
    router.register::<SessionActor, M, _, _, _, _>(
        actor.id(),
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        move |request, message| {
            let weak = weak.clone();
            let store = store.clone();
            async move {
                let actor = weak
                    .upgrade()
                    .ok_or_else(|| AgentError::RemoteActor("session stopped".into()))?;
                let peer = Arc::new(PeerTurn::from_request(&request, store)?);
                actor
                    .ask(crate::agent::session_actor::PeerInteraction { peer, message })
                    .mailbox_timeout(Duration::from_secs(10))
                    .reply_timeout(Duration::from_secs(10))
                    .send()
                    .await
                    .map_err(local_session_error)
            }
        },
    );
}

pub(super) fn register_session_turns(
    router: &InboundRouter,
    store: Arc<dyn GrantStore>,
    actor: &ActorRef<SessionActor>,
) {
    register_peer_session_ask::<messages::Prompt, _>(
        router,
        &store,
        actor,
        Duration::from_secs(600),
        InboundDelivery::Ask,
        |msg| &mut msg.authority,
    );
    register_peer_session_ask::<messages::SubmitSessionInput, _>(
        router,
        &store,
        actor,
        Duration::from_secs(10),
        InboundDelivery::Ask,
        |msg| &mut msg.authority,
    );
    register_peer_session_ask::<messages::DiscardQueuedInput, _>(
        router,
        &store,
        actor,
        Duration::from_secs(10),
        InboundDelivery::Ask,
        |msg| &mut msg.authority,
    );
    register_peer_session_ask::<messages::GetRuntimeStatus, _>(
        router,
        &store,
        actor,
        Duration::from_secs(10),
        InboundDelivery::Ask,
        |msg| &mut msg.authority,
    );
    register_peer_session_ask::<messages::Cancel, _>(
        router,
        &store,
        actor,
        Duration::from_secs(10),
        InboundDelivery::LegacyTellV1Ask,
        |msg| &mut msg.authority,
    );
}

fn register_peer_session_ask<M, R>(
    router: &InboundRouter,
    store: &Arc<dyn GrantStore>,
    actor: &ActorRef<SessionActor>,
    timeout: Duration,
    delivery: InboundDelivery,
    authority: fn(&mut M) -> &mut TurnAuthority,
) where
    SessionActor: RemoteMessage<M> + Message<M>,
    M: serde::de::DeserializeOwned + Send + 'static,
    <SessionActor as Message<M>>::Reply: Reply<Ok = R>,
    <<SessionActor as Message<M>>::Reply as Reply>::Error: Into<AgentError>,
    R: serde::Serialize + Send + 'static,
{
    let weak = actor.downgrade();
    let store = store.clone();
    let handle = move |origin: PeerRequest, mut message: M| {
        let weak = weak.clone();
        let store = store.clone();
        async move {
            let actor = weak
                .upgrade()
                .ok_or_else(|| AgentError::RemoteActor("session stopped".into()))?;
            *authority(&mut message) =
                TurnAuthority::Peer(Arc::new(PeerTurn::from_request(&origin, store)?));
            actor
                .ask(message)
                .mailbox_timeout(Duration::from_secs(10))
                .reply_timeout(timeout)
                .send()
                .await
                .map_err(|error| local_session_error(error.map_err(Into::into)))
        }
    };
    router.register::<SessionActor, M, _, _, _, _>(
        actor.id(),
        delivery,
        InboundAccess::Sessions,
        handle,
    );
}

pub(super) fn register_file_proxy(router: &InboundRouter, actor: &ActorRef<SessionActor>) {
    use crate::agent::file_proxy::FileProxyError;
    use crate::agent::session_actor::{GetPeerFileIndex, ReadPeerFile};

    let weak = actor.downgrade();
    let index = move |_, _: messages::GetFileIndex| {
        let weak = weak.clone();
        async move {
            let actor = weak.upgrade().ok_or_else(|| {
                file_actor_unavailable(querymt_remote::DeliveryCertainty::NotDelivered)
            })?;
            actor
                .ask(GetPeerFileIndex)
                .mailbox_timeout(Duration::from_secs(3))
                .reply_timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(file_proxy_send_error)
        }
    };
    let weak = actor.downgrade();
    let read = move |_, message| {
        let weak = weak.clone();
        async move {
            let actor = weak.upgrade().ok_or_else(|| {
                file_actor_unavailable(querymt_remote::DeliveryCertainty::NotDelivered)
            })?;
            actor
                .ask(ReadPeerFile(message))
                .mailbox_timeout(Duration::from_secs(3))
                .reply_timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(file_proxy_send_error)
        }
    };
    router.register::<SessionActor, messages::GetFileIndex, _, _, _, _>(
        actor.id(),
        InboundDelivery::Ask,
        InboundAccess::Filesystem,
        index,
    );
    router.register::<SessionActor, messages::ReadRemoteFile, _, _, _, _>(
        actor.id(),
        InboundDelivery::Ask,
        InboundAccess::Filesystem,
        read,
    );
}

fn file_actor_unavailable(
    delivery: querymt_remote::DeliveryCertainty,
) -> crate::agent::file_proxy::FileProxyError {
    crate::agent::file_proxy::FileProxyError::from_transport_failure(
        querymt_remote::RemoteTransportFailure::new(
            querymt_remote::RemoteTransportFailureKind::ActorUnavailable,
            delivery,
            "session stopped",
        ),
    )
}

fn file_proxy_send_error<M>(
    error: kameo::error::SendError<M, crate::agent::file_proxy::FileProxyError>,
) -> crate::agent::file_proxy::FileProxyError {
    use crate::agent::file_proxy::FileProxyError;
    match error {
        kameo::error::SendError::HandlerError(error) => error,
        kameo::error::SendError::ActorNotRunning(_)
        | kameo::error::SendError::ActorRestarting(_) => {
            file_actor_unavailable(querymt_remote::DeliveryCertainty::NotDelivered)
        }
        kameo::error::SendError::ActorStopped => {
            file_actor_unavailable(querymt_remote::DeliveryCertainty::Unknown)
        }
        other => FileProxyError::ActorSend(other.to_string()),
    }
}

pub(super) fn register_session_subscriptions(
    router: &InboundRouter,
    mesh: &querymt_remote::MeshHandle,
    store: std::sync::Arc<dyn querymt_remote::peer_access::GrantStore>,
    actor: &ActorRef<SessionActor>,
) {
    let weak = actor.downgrade();
    let egress = mesh.sharing_egress();
    let subscribe = move |origin: PeerRequest, message| {
        let weak = weak.clone();
        let subscriber = PeerSubscription {
            egress: egress.clone(),
            store: store.clone(),
            changes: origin.take_policy_changes(),
            origin,
        };
        async move {
            let actor = weak
                .upgrade()
                .ok_or_else(|| AgentError::RemoteActor("session stopped".into()))?;
            actor
                .ask(SubscribePeerEvents {
                    message,
                    subscriber,
                })
                .mailbox_timeout(Duration::from_secs(3))
                .reply_timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(local_session_error)
        }
    };
    router.register::<SessionActor, messages::SubscribeEvents, _, _, _, _>(
        actor.id(),
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        subscribe,
    );

    let weak = actor.downgrade();
    let unsubscribe = move |origin: querymt_remote::mesh_ingress::PeerRequest, message| {
        let weak = weak.clone();
        async move {
            let actor = weak
                .upgrade()
                .ok_or_else(|| AgentError::RemoteActor("session stopped".into()))?;
            actor
                .ask(UnsubscribePeerEvents {
                    message,
                    peer: *origin.key().peer.as_peer_id(),
                })
                .mailbox_timeout(Duration::from_secs(3))
                .reply_timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(local_session_error)
        }
    };
    router.register::<SessionActor, messages::UnsubscribeEvents, _, _, _, _>(
        actor.id(),
        InboundDelivery::Ask,
        InboundAccess::Sessions,
        unsubscribe,
    );
}

fn local_session_error<M>(error: kameo::error::SendError<M, AgentError>) -> AgentError {
    match error {
        kameo::error::SendError::HandlerError(error) => error,
        other => AgentError::RemoteActor(other.to_string()),
    }
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
    messages::SetProvider,
    "querymt::SetProvider",
    REG_SET_PROVIDER
);
remote_msg_impl!(
    messages::SetLlmConfig,
    "querymt::SetLlmConfig",
    REG_SET_LLM_CONFIG
);
remote_msg_impl!(
    messages::SetToolPolicy,
    "querymt::SetToolPolicy",
    REG_SET_TOOL_POLICY
);
remote_msg_impl!(
    messages::SetAllowedTools,
    "querymt::SetAllowedTools",
    REG_SET_ALLOWED_TOOLS
);
remote_msg_impl!(
    messages::ClearAllowedTools,
    "querymt::ClearAllowedTools",
    REG_CLEAR_ALLOWED_TOOLS
);
remote_msg_impl!(
    messages::SetDeniedTools,
    "querymt::SetDeniedTools",
    REG_SET_DENIED_TOOLS
);
remote_msg_impl!(
    messages::ClearDeniedTools,
    "querymt::ClearDeniedTools",
    REG_CLEAR_DENIED_TOOLS
);
remote_msg_impl!(messages::ExtMethod, "querymt::ExtMethod", REG_EXT_METHOD);
remote_msg_impl!(
    messages::ExtNotification,
    "querymt::ExtNotification",
    REG_EXT_NOTIFICATION
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
