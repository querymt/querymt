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
use crate::agent::remote::event_relay::{EventRelayActor, RelayedEvent};
use crate::agent::session_actor::SessionActor;
use kameo::remote::_internal;

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
remote_msg_impl!(messages::Cancel, "querymt::Cancel", REG_CANCEL);
remote_msg_impl!(messages::SetMode, "querymt::SetMode", REG_SET_MODE);
remote_msg_impl!(messages::GetMode, "querymt::GetMode", REG_GET_MODE);
remote_msg_impl!(messages::Undo, "querymt::Undo", REG_UNDO);
remote_msg_impl!(messages::Redo, "querymt::Redo", REG_REDO);
remote_msg_impl!(
    messages::SetSessionModel,
    "querymt::SetSessionModel",
    REG_SET_SESSION_MODEL
);
remote_msg_impl!(messages::GetHistory, "querymt::GetHistory", REG_GET_HISTORY);
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
