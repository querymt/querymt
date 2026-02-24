//! `RegistryExchangeActor` — lightweight actor for pre-warming the actor cache.
//!
//! `CachedMeshTransport` spawns one `RegistryExchangeActor` per node and
//! registers it in the DHT under `registry_exchange::peer::{peer_id}`.
//!
//! When a new peer is discovered, `CachedMeshTransport`:
//! 1. Looks up the new peer's `RegistryExchangeActor`.
//! 2. Sends `GetRegistrations` to obtain the peer's locally-registered actors.
//! 3. For each returned registration, does a Kademlia lookup to get a live
//!    `RemoteActorRef` and stores it in the local cache.
//!
//! This eliminates Kademlia latency for the first lookup of every actor:
//! by the time a user triggers an LLM call, the cache is already warm.

use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

// ── Wire types ────────────────────────────────────────────────────────────────

/// One entry in the local registration table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistrationEntry {
    /// The DHT name the actor is registered under.
    pub dht_name: String,
    /// The raw 64-bit sequence ID of the `ActorId` (stable, serialisable).
    pub actor_sequence_id: u64,
}

/// Request the full registration table of the remote peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRegistrations;

/// Notification pushed to all known peers when a new actor is registered
/// locally. Recipients can pre-warm their cache without waiting for the
/// next `GetRegistrations` poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyRegistration {
    pub entry: RegistrationEntry,
}

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Lightweight kameo actor that maintains the local registration table and
/// serves it to peers on request.
///
/// Registered in the DHT as `registry_exchange::peer::{peer_id}`.
#[derive(Actor, Clone)]
pub struct RegistryExchangeActor {
    /// All locally registered actors, in registration order.
    registrations: Arc<RwLock<Vec<RegistrationEntry>>>,
}

impl RegistryExchangeActor {
    /// Create a new actor backed by the given shared registration list.
    ///
    /// The same `Arc<RwLock<...>>` is also held by `CachedMeshTransport` so
    /// both can see new registrations without message-passing overhead.
    pub fn new(registrations: Arc<RwLock<Vec<RegistrationEntry>>>) -> Self {
        Self { registrations }
    }

    /// DHT name for this actor given the owning node's peer id.
    pub fn dht_name(peer_id: &impl std::fmt::Display) -> String {
        format!("registry_exchange::peer::{}", peer_id)
    }
}

// ── Message handlers ──────────────────────────────────────────────────────────

impl Message<GetRegistrations> for RegistryExchangeActor {
    type Reply = Vec<RegistrationEntry>;

    async fn handle(
        &mut self,
        _msg: GetRegistrations,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.registrations
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
    }
}

impl Message<NotifyRegistration> for RegistryExchangeActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: NotifyRegistration,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        if let Ok(mut reg) = self.registrations.write() {
            // Avoid duplicates (idempotent re-registration).
            if !reg.iter().any(|e| e.dht_name == msg.entry.dht_name) {
                reg.push(msg.entry);
            }
        }
    }
}

// ── kameo remote wiring ───────────────────────────────────────────────────────

impl kameo::remote::RemoteActor for RegistryExchangeActor {
    const REMOTE_ID: &'static str = "querymt::RegistryExchangeActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static REGISTRY_EXCHANGE_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <RegistryExchangeActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<RegistryExchangeActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<RegistryExchangeActor>(
                actor_id, sibling_id,
            ))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<RegistryExchangeActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

// Register remote messages using the same macro pattern as provider_host.rs.
macro_rules! remote_exchange_msg_impl {
    ($msg_ty:ty, $remote_id:expr, $static_name:ident) => {
        impl kameo::remote::RemoteMessage<$msg_ty> for RegistryExchangeActor {
            const REMOTE_ID: &'static str = $remote_id;
        }

        #[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
        #[linkme(crate = _internal::linkme)]
        static $static_name: (
            _internal::RemoteMessageRegistrationID<'static>,
            _internal::RemoteMessageFns,
        ) = (
            _internal::RemoteMessageRegistrationID {
                actor_remote_id: <RegistryExchangeActor as kameo::remote::RemoteActor>::REMOTE_ID,
                message_remote_id: <RegistryExchangeActor as kameo::remote::RemoteMessage<
                    $msg_ty,
                >>::REMOTE_ID,
            },
            _internal::RemoteMessageFns {
                ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
                    Box::pin(_internal::ask::<RegistryExchangeActor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                        reply_timeout,
                    ))
                }) as _internal::RemoteAskFn,
                try_ask: (|actor_id, msg, reply_timeout| {
                    Box::pin(_internal::try_ask::<RegistryExchangeActor, $msg_ty>(
                        actor_id,
                        msg,
                        reply_timeout,
                    ))
                }) as _internal::RemoteTryAskFn,
                tell: (|actor_id, msg, mailbox_timeout| {
                    Box::pin(_internal::tell::<RegistryExchangeActor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                    ))
                }) as _internal::RemoteTellFn,
                try_tell: (|actor_id, msg| {
                    Box::pin(_internal::try_tell::<RegistryExchangeActor, $msg_ty>(
                        actor_id, msg,
                    ))
                }) as _internal::RemoteTryTellFn,
            },
        );
    };
}

remote_exchange_msg_impl!(
    GetRegistrations,
    "querymt::RegistryExchange::GetRegistrations",
    REG_GET_REGISTRATIONS
);
remote_exchange_msg_impl!(
    NotifyRegistration,
    "querymt::RegistryExchange::NotifyRegistration",
    REG_NOTIFY_REGISTRATION
);
