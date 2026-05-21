//! `SessionStreamRouterActor` — stable per-session router for iroh/mobile resilience.
//!
//! This actor lives for the duration of a session or app runtime, not per-request.
//! It receives routed stream relay messages from provider hosts and forwards them
//! to the appropriate local consumer for each request.
//!
//! ## Key Features
//!
//! - Request routing: Maps `request_id` → local stream sink
//! - Bounded replay buffers: Allows UI/session to reattach after transient disconnects
//! - Attach/detach semantics: UI can attach/detach consumers per request
//! - Lease-based cleanup: Drops request state on terminal messages or expiry
//!
//! ## Why a Router for iroh/mobile
//!
//! Direct per-request receiver handoff (Phase 1/2) works well for LAN, but for
//! iroh/mobile scenarios:
//!
//! - If phone loses connectivity but app survives, provider buffers during reconnect
//! - iroh reconnects by peer id, provider resumes sending to same router
//! - Router forwards/buffers locally until UI consumer is attached
//! - Clearer recovery path than trying to resume with per-request DHT names

use querymt::chat::StreamChunk;

use super::provider_host::{StreamRelayMessage, relay_message_is_terminal};
use crate::error::AgentError;
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Maximum number of messages to buffer per request when no consumer is attached.
const DEFAULT_REPLAY_BUFFER_SIZE: usize = 1000;

/// Default TTL for request state after terminal message (seconds).
const DEFAULT_REQUEST_TTL_SECS: u64 = 300; // 5 minutes

// ── Wire types ────────────────────────────────────────────────────────────────

/// Routed stream relay message sent from `ProviderHostActor` to `SessionStreamRouterActor`.
///
/// Carries the `request_id` so the router can forward to the correct local consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedStreamRelayMessage {
    /// The request this message belongs to.
    pub request_id: String,
    /// The actual relay payload.
    pub message: StreamRelayMessage,
}

/// Attach a local consumer to receive chunks for a specific request.
///
/// This is typically called by UI/session code when it wants to receive stream updates.
/// Note: This is a local-only message (not serializable) since mpsc::Sender cannot be
/// serialized. Use the local ActorRef for attach/detach operations.
#[derive(Debug)]
pub struct AttachStreamConsumer {
    /// The request to attach to.
    pub request_id: String,
    /// Channel to send stream messages to.
    pub consumer_tx: mpsc::Sender<StreamRelayMessage>,
}

/// Detach the current consumer for a request.
///
/// Messages will continue to be buffered (up to the replay buffer limit)
/// until a new consumer attaches or the request expires.
/// Note: This is a local-only message. Use the local ActorRef for detach operations.
#[derive(Debug, Clone)]
pub struct DetachStreamConsumer {
    /// The request to detach from.
    pub request_id: String,
}

/// Get status of a specific request or all requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRouterStatus {
    /// If provided, return status for this specific request.
    pub request_id: Option<String>,
}

/// Status information for a routed request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedRequestStatus {
    pub request_id: String,
    pub has_consumer: bool,
    pub buffered_messages: usize,
    pub phase: RequestPhase,
    pub created_at_elapsed_ms: u64,
    pub last_message_at_elapsed_ms: u64,
}

/// Phase of a routed request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestPhase {
    /// Waiting for first message from provider.
    AwaitingStream,
    /// Actively receiving chunks.
    Streaming,
    /// Consumer disconnected, buffering messages.
    ConsumerDisconnected,
    /// Request completed normally.
    Completed,
    /// Request failed with error.
    Failed,
    /// Request cancelled.
    Cancelled,
}

/// Determine the terminal phase for a relay message, if it is terminal.
///
/// Returns `Some(RequestPhase::Completed)` for successful stream completion
/// (a `Done` chunk or a `ChunkBatch` containing `Done`), and
/// `Some(RequestPhase::Failed)` for error/transport-failure messages.
/// Returns `None` for non-terminal messages.
fn terminal_request_phase(message: &StreamRelayMessage) -> Option<RequestPhase> {
    match message {
        StreamRelayMessage::Chunk(StreamChunk::Done { .. }) => Some(RequestPhase::Completed),
        StreamRelayMessage::ChunkBatch(chunks)
            if chunks.iter().any(|c| matches!(c, StreamChunk::Done { .. })) =>
        {
            Some(RequestPhase::Completed)
        }
        StreamRelayMessage::ProviderError { .. } | StreamRelayMessage::TransportFailed { .. } => {
            Some(RequestPhase::Failed)
        }
        _ => None,
    }
}

impl fmt::Display for RequestPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestPhase::AwaitingStream => write!(f, "awaiting_stream"),
            RequestPhase::Streaming => write!(f, "streaming"),
            RequestPhase::ConsumerDisconnected => write!(f, "consumer_disconnected"),
            RequestPhase::Completed => write!(f, "completed"),
            RequestPhase::Failed => write!(f, "failed"),
            RequestPhase::Cancelled => write!(f, "cancelled"),
        }
    }
}

// ── Internal state ────────────────────────────────────────────────────────────

struct RoutedRequest {
    request_id: String,
    phase: RequestPhase,
    created_at: Instant,
    last_message_at: Instant,
    consumer_tx: Option<mpsc::Sender<StreamRelayMessage>>,
    replay_buffer: VecDeque<StreamRelayMessage>,
    replay_buffer_size: usize,
}

impl RoutedRequest {
    fn new(request_id: String, replay_buffer_size: usize) -> Self {
        let now = Instant::now();
        Self {
            request_id,
            phase: RequestPhase::AwaitingStream,
            created_at: now,
            last_message_at: now,
            consumer_tx: None,
            replay_buffer: VecDeque::with_capacity(replay_buffer_size.min(64)),
            replay_buffer_size,
        }
    }

    fn status(&self) -> RoutedRequestStatus {
        let now = Instant::now();
        RoutedRequestStatus {
            request_id: self.request_id.clone(),
            has_consumer: self.consumer_tx.is_some(),
            buffered_messages: self.replay_buffer.len(),
            phase: self.phase,
            created_at_elapsed_ms: now.duration_since(self.created_at).as_millis() as u64,
            last_message_at_elapsed_ms: now.duration_since(self.last_message_at).as_millis() as u64,
        }
    }

    /// Try to send a message to the consumer, buffering if send fails or no consumer.
    ///
    /// Optimized to avoid cloning on successful send (important for large ChunkBatch messages).
    ///
    /// When a terminal message (`Done`, `ProviderError`, `TransportFailed`, or a
    /// `ChunkBatch` containing `Done`) is delivered to the consumer, the consumer
    /// sender is dropped so the receiver sees EOF.  This mirrors the old
    /// `StreamReceiverActor` lifecycle where the actor stopped after forwarding a
    /// terminal message, closing the mpsc channel.
    fn deliver_or_buffer(&mut self, message: StreamRelayMessage) {
        self.last_message_at = Instant::now();

        // Check if this message is terminal BEFORE potentially moving it.
        let is_terminal = relay_message_is_terminal(&message);
        let terminal_phase = terminal_request_phase(&message);

        // Update phase based on message type
        match &message {
            StreamRelayMessage::Chunk(_) | StreamRelayMessage::ChunkBatch(_)
                if self.phase == RequestPhase::AwaitingStream =>
            {
                self.phase = RequestPhase::Streaming;
            }
            StreamRelayMessage::ProviderError { .. }
            | StreamRelayMessage::TransportFailed { .. } => {
                self.phase = RequestPhase::Failed;
            }
            _ => {}
        }

        // Try to send to attached consumer (move message, not clone)
        let message = if let Some(tx) = &self.consumer_tx {
            match tx.try_send(message) {
                Ok(()) => {
                    // Message was delivered directly to the consumer.
                    if is_terminal {
                        if let Some(phase) = terminal_phase {
                            self.phase = phase;
                        }
                        // Drop the consumer sender so the mpsc receiver sees EOF.
                        // This is critical: without it, the requester-side
                        // ReceiverStream never ends and the execution loop
                        // hangs after consuming Done waiting for more chunks.
                        self.consumer_tx = None;
                    }
                    return;
                }
                Err(mpsc::error::TrySendError::Full(msg)) => {
                    // Consumer too slow, buffer the message
                    self.phase = RequestPhase::ConsumerDisconnected;
                    msg
                }
                Err(mpsc::error::TrySendError::Closed(msg)) => {
                    // Consumer dropped, remove it and buffer
                    self.consumer_tx = None;
                    self.phase = RequestPhase::ConsumerDisconnected;
                    msg
                }
            }
        } else {
            message
        };

        // Buffer the message (with bounded size)
        if self.replay_buffer.len() >= self.replay_buffer_size {
            self.replay_buffer.pop_front();
        }
        self.replay_buffer.push_back(message);

        // Check if terminal — update phase and note that consumer should
        // be detached when/if it re-attaches.
        if relay_message_is_terminal(self.replay_buffer.back().unwrap()) {
            if let Some(phase) = terminal_phase {
                self.phase = phase;
            }
        }
    }

    /// Check if this request is terminal and can be cleaned up.
    fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            RequestPhase::Completed | RequestPhase::Failed | RequestPhase::Cancelled
        )
    }
}

// ── SessionStreamRouterActor ──────────────────────────────────────────────────

/// Stable per-session router for iroh/mobile resilience.
///
/// Lives for the session or app runtime. Routes stream relay messages from
/// provider hosts to local consumers based on `request_id`.
#[derive(Actor)]
pub struct SessionStreamRouterActor {
    /// Map of active requests.
    requests: Arc<Mutex<HashMap<String, RoutedRequest>>>,
    /// Default replay buffer size for new requests.
    replay_buffer_size: usize,
    /// TTL for request state after terminal message.
    request_ttl: Duration,
}

impl SessionStreamRouterActor {
    pub fn new(replay_buffer_size: Option<usize>, request_ttl_secs: Option<u64>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(HashMap::new())),
            replay_buffer_size: replay_buffer_size.unwrap_or(DEFAULT_REPLAY_BUFFER_SIZE),
            request_ttl: Duration::from_secs(request_ttl_secs.unwrap_or(DEFAULT_REQUEST_TTL_SECS)),
        }
    }

    /// Register a new request that we expect to receive streams for.
    ///
    /// This should be called when sending a `ProviderStreamRequest` to pre-register
    /// the request in the router.
    pub fn register_request(&self, request_id: String) {
        let mut requests = self.requests.lock();
        requests.insert(
            request_id.clone(),
            RoutedRequest::new(request_id, self.replay_buffer_size),
        );
    }

    /// Remove terminal requests older than the TTL.
    fn cleanup_expired_requests(&self) {
        let mut requests = self.requests.lock();
        requests.retain(|_, req| {
            if !req.is_terminal() {
                return true;
            }
            req.last_message_at.elapsed() < self.request_ttl
        });
    }

    /// Get status for one or all requests.
    fn get_status(&self, request_id: Option<&str>) -> Vec<RoutedRequestStatus> {
        let requests = self.requests.lock();
        if let Some(request_id) = request_id {
            requests
                .get(request_id)
                .map(|req| vec![req.status()])
                .unwrap_or_default()
        } else {
            requests.values().map(RoutedRequest::status).collect()
        }
    }
}

// ── Message handlers ──────────────────────────────────────────────────────────

impl Message<RoutedStreamRelayMessage> for SessionStreamRouterActor {
    type Reply = ();

    #[tracing::instrument(
        name = "remote.session_router.relay",
        skip(self, _ctx),
        fields(
            request_id = %msg.request_id,
            message_type = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: RoutedStreamRelayMessage,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let message_type = match &msg.message {
            StreamRelayMessage::Chunk(_) => "chunk",
            StreamRelayMessage::ChunkBatch(_) => "chunk_batch",
            StreamRelayMessage::Heartbeat { .. } => "heartbeat",
            StreamRelayMessage::ProviderError { .. } => "provider_error",
            StreamRelayMessage::TransportDisconnected { .. } => "transport_disconnected",
            StreamRelayMessage::TransportReconnected { .. } => "transport_reconnected",
            StreamRelayMessage::TransportFailed { .. } => "transport_failed",
        };
        tracing::Span::current().record("message_type", message_type);

        let mut requests = self.requests.lock();

        // Get or create the routed request
        let request = requests
            .entry(msg.request_id.clone())
            .or_insert_with(|| RoutedRequest::new(msg.request_id.clone(), self.replay_buffer_size));

        // Deliver or buffer the message
        request.deliver_or_buffer(msg.message);

        tracing::trace!(
            target: "remote::session_router",
            request_id = %msg.request_id,
            phase = %request.phase,
            has_consumer = request.consumer_tx.is_some(),
            buffered = request.replay_buffer.len(),
            "routed message delivered or buffered"
        );
    }
}

impl Message<AttachStreamConsumer> for SessionStreamRouterActor {
    type Reply = Result<(), AgentError>;

    #[tracing::instrument(
        name = "remote.session_router.attach",
        skip(self, _ctx, msg),
        fields(request_id = %msg.request_id)
    )]
    async fn handle(
        &mut self,
        msg: AttachStreamConsumer,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let request_id = msg.request_id.clone();
        let consumer_tx = msg.consumer_tx;

        // Cleanup expired requests before attaching new consumer
        self.cleanup_expired_requests();

        // Phase 1: Attach the consumer and extract buffered messages (under lock)
        let (buffered, has_buffered) = {
            let mut requests = self.requests.lock();

            let request = requests
                .entry(request_id.clone())
                .or_insert_with(|| RoutedRequest::new(request_id.clone(), self.replay_buffer_size));

            // Attach the consumer
            request.consumer_tx = Some(consumer_tx.clone());

            // If we're in consumer disconnected state, go back to streaming
            if request.phase == RequestPhase::ConsumerDisconnected {
                request.phase = if request.replay_buffer.is_empty() {
                    RequestPhase::AwaitingStream
                } else {
                    RequestPhase::Streaming
                };
            }

            tracing::info!(
                target: "remote::session_router",
                request_id = %request_id,
                buffered_messages = request.replay_buffer.len(),
                "consumer attached"
            );

            // Extract buffered messages
            let buffered: Vec<_> = request.replay_buffer.drain(..).collect();
            let has_buffered = !buffered.is_empty();

            // Update phase based on what we're flushing
            if has_buffered {
                request.phase = RequestPhase::Streaming;
            }

            (buffered, has_buffered)
        }; // Lock released here

        // Phase 2: Send buffered messages outside the lock
        if has_buffered {
            let mut last_was_terminal = false;
            for message in &buffered {
                if relay_message_is_terminal(message) {
                    last_was_terminal = true;
                } else {
                    last_was_terminal = false;
                }
            }
            for message in buffered {
                if consumer_tx.send(message).await.is_err() {
                    tracing::warn!(
                        target: "remote::session_router",
                        request_id = %request_id,
                        "consumer dropped during buffer flush"
                    );
                    // Re-lock and update state
                    let mut requests = self.requests.lock();
                    if let Some(req) = requests.get_mut(&request_id) {
                        req.consumer_tx = None;
                        req.phase = RequestPhase::ConsumerDisconnected;
                    }
                    return Err(AgentError::Internal(
                        "consumer dropped during attach".to_string(),
                    ));
                }
            }
            // If the last replayed message was terminal, drop the consumer
            // sender so the receiver sees EOF — same lifecycle as direct
            // terminal delivery in deliver_or_buffer.
            if last_was_terminal {
                let mut requests = self.requests.lock();
                if let Some(req) = requests.get_mut(&request_id) {
                    req.consumer_tx = None;
                    // Phase was already set correctly during buffering.
                }
            }
        }

        Ok(())
    }
}

impl Message<DetachStreamConsumer> for SessionStreamRouterActor {
    type Reply = ();

    #[tracing::instrument(
        name = "remote.session_router.detach",
        skip(self, _ctx),
        fields(request_id = %msg.request_id)
    )]
    async fn handle(
        &mut self,
        msg: DetachStreamConsumer,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        // Cleanup expired requests after detaching consumer
        self.cleanup_expired_requests();

        let mut requests = self.requests.lock();

        if let Some(request) = requests.get_mut(&msg.request_id) {
            request.consumer_tx = None;
            if !request.is_terminal() {
                request.phase = RequestPhase::ConsumerDisconnected;
            }

            tracing::info!(
                target: "remote::session_router",
                request_id = %msg.request_id,
                phase = %request.phase,
                buffered = request.replay_buffer.len(),
                "consumer detached"
            );
        }
    }
}

impl Message<GetRouterStatus> for SessionStreamRouterActor {
    type Reply = Vec<RoutedRequestStatus>;

    async fn handle(
        &mut self,
        msg: GetRouterStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.get_status(msg.request_id.as_deref())
    }
}

// ── RemoteActor + RemoteMessage registrations ─────────────────────────────────

impl kameo::remote::RemoteActor for SessionStreamRouterActor {
    const REMOTE_ID: &'static str = "querymt::SessionStreamRouterActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static SESSION_STREAM_ROUTER_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <SessionStreamRouterActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<SessionStreamRouterActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<SessionStreamRouterActor>(
                actor_id, sibling_id,
            ))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<SessionStreamRouterActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

macro_rules! remote_router_msg_impl {
    ($actor:ty, $msg_ty:ty, $remote_id:expr, $static_name:ident) => {
        impl kameo::remote::RemoteMessage<$msg_ty> for $actor {
            const REMOTE_ID: &'static str = $remote_id;
        }

        #[_internal::linkme::distributed_slice(_internal::REMOTE_MESSAGES)]
        #[linkme(crate = _internal::linkme)]
        static $static_name: (
            _internal::RemoteMessageRegistrationID<'static>,
            _internal::RemoteMessageFns,
        ) = (
            _internal::RemoteMessageRegistrationID {
                actor_remote_id: <$actor as kameo::remote::RemoteActor>::REMOTE_ID,
                message_remote_id: <$actor as kameo::remote::RemoteMessage<$msg_ty>>::REMOTE_ID,
            },
            _internal::RemoteMessageFns {
                ask: (|actor_id, msg, mailbox_timeout, reply_timeout| {
                    Box::pin(_internal::ask::<$actor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                        reply_timeout,
                    ))
                }) as _internal::RemoteAskFn,
                try_ask: (|actor_id, msg, reply_timeout| {
                    Box::pin(_internal::try_ask::<$actor, $msg_ty>(
                        actor_id,
                        msg,
                        reply_timeout,
                    ))
                }) as _internal::RemoteTryAskFn,
                tell: (|actor_id, msg, mailbox_timeout| {
                    Box::pin(_internal::tell::<$actor, $msg_ty>(
                        actor_id,
                        msg,
                        mailbox_timeout,
                    ))
                }) as _internal::RemoteTellFn,
                try_tell: (|actor_id, msg| {
                    Box::pin(_internal::try_tell::<$actor, $msg_ty>(actor_id, msg))
                }) as _internal::RemoteTryTellFn,
            },
        );
    };
}

// SessionStreamRouterActor messages
remote_router_msg_impl!(
    SessionStreamRouterActor,
    RoutedStreamRelayMessage,
    "querymt::RoutedStreamRelayMessage",
    REG_ROUTED_STREAM_RELAY_MESSAGE
);
// Note: AttachStreamConsumer and DetachStreamConsumer are local-only messages
// (not serializable) since they contain mpsc::Sender. Use local ActorRef for these operations.
remote_router_msg_impl!(
    SessionStreamRouterActor,
    GetRouterStatus,
    "querymt::GetRouterStatus",
    REG_GET_ROUTER_STATUS
);

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::actor::Spawn;
    use querymt::chat::StreamChunk;
    use tokio::sync::mpsc;

    fn create_test_router() -> SessionStreamRouterActor {
        SessionStreamRouterActor::new(Some(10), Some(60)) // 10 buffer, 60s TTL
    }

    #[tokio::test]
    async fn test_register_request_creates_entry() {
        let router = create_test_router();
        let request_id = "test-request-1".to_string();

        router.register_request(request_id.clone());

        let status = router.get_status(Some(&request_id));
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].request_id, request_id);
        assert_eq!(status[0].phase, RequestPhase::AwaitingStream);
        assert!(!status[0].has_consumer);
        assert_eq!(status[0].buffered_messages, 0);
    }

    #[tokio::test]
    async fn test_attach_consumer_to_registered_request() {
        let router = create_test_router();
        let request_id = "test-request-2".to_string();
        router.register_request(request_id.clone());

        let (tx, _rx) = mpsc::channel(100);
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .consumer_tx = Some(tx);

        let status = router.get_status(Some(&request_id));
        assert!(status[0].has_consumer);
    }

    #[tokio::test]
    async fn test_attach_creates_request_if_not_registered() {
        let router = create_test_router();
        let request_id = "test-request-3".to_string();

        let (tx, mut rx) = mpsc::channel(100);
        router.requests.lock().insert(
            request_id.clone(),
            RoutedRequest::new(request_id.clone(), router.replay_buffer_size),
        );
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .consumer_tx = Some(tx);

        // Send a message - should be received
        let message = StreamRelayMessage::Chunk(StreamChunk::Text("test".to_string()));
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(message);

        let received = rx.recv().await.unwrap();
        match received {
            StreamRelayMessage::Chunk(StreamChunk::Text(content)) => {
                assert_eq!(content, "test");
            }
            _ => panic!("Expected text chunk"),
        }
    }

    #[tokio::test]
    async fn test_buffer_message_when_no_consumer() {
        let router = create_test_router();
        let request_id = "test-request-4".to_string();
        router.register_request(request_id.clone());

        let message = StreamRelayMessage::Chunk(StreamChunk::Text("buffered".to_string()));
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(message);

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].buffered_messages, 1);
        assert_eq!(status[0].phase, RequestPhase::Streaming);
    }

    #[tokio::test]
    async fn test_buffer_overflow_evicts_oldest() {
        let router = SessionStreamRouterActor::new(Some(3), Some(60)); // Small buffer
        let request_id = "test-request-5".to_string();
        router.register_request(request_id.clone());

        // Fill buffer to capacity
        for i in 0..3 {
            let message = StreamRelayMessage::Chunk(StreamChunk::Text(format!("msg-{}", i)));
            router
                .requests
                .lock()
                .get_mut(&request_id)
                .unwrap()
                .deliver_or_buffer(message);
        }

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].buffered_messages, 3);

        // Add one more - should evict oldest
        let message = StreamRelayMessage::Chunk(StreamChunk::Text("msg-3".to_string()));
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(message);

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].buffered_messages, 3); // Still at capacity

        // Attach consumer and verify we get messages 1, 2, 3 (not 0)
        let (tx, mut rx) = mpsc::channel(100);
        {
            let mut requests = router.requests.lock();
            let req = requests.get_mut(&request_id).unwrap();
            req.consumer_tx = Some(tx.clone());
            let buffered: Vec<_> = req.replay_buffer.drain(..).collect();
            for msg in buffered {
                tx.try_send(msg).unwrap();
            }
        }

        let msg1 = rx.recv().await.unwrap();
        match msg1 {
            StreamRelayMessage::Chunk(StreamChunk::Text(content)) => {
                assert_eq!(content, "msg-1")
            }
            _ => panic!("Expected msg-1"),
        }

        let msg2 = rx.recv().await.unwrap();
        match msg2 {
            StreamRelayMessage::Chunk(StreamChunk::Text(content)) => {
                assert_eq!(content, "msg-2")
            }
            _ => panic!("Expected msg-2"),
        }

        let msg3 = rx.recv().await.unwrap();
        match msg3 {
            StreamRelayMessage::Chunk(StreamChunk::Text(content)) => {
                assert_eq!(content, "msg-3")
            }
            _ => panic!("Expected msg-3"),
        }
    }

    #[tokio::test]
    async fn test_terminal_message_sets_completed_phase() {
        let router = create_test_router();
        let request_id = "test-request-6".to_string();
        router.register_request(request_id.clone());

        let message = StreamRelayMessage::Chunk(StreamChunk::Done {
            finish_reason: querymt::chat::FinishReason::Stop,
        });
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(message);

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].phase, RequestPhase::Completed);
    }

    #[tokio::test]
    async fn test_error_message_sets_failed_phase() {
        let router = create_test_router();
        let request_id = "test-request-7".to_string();
        router.register_request(request_id.clone());

        let message = StreamRelayMessage::ProviderError {
            error: querymt::error::LLMErrorPayload::ProviderError {
                message: "test error message".to_string(),
            },
        };
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(message);

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].phase, RequestPhase::Failed);
    }

    #[tokio::test]
    async fn test_detach_sets_consumer_disconnected_phase() {
        let router = create_test_router();
        let request_id = "test-request-8".to_string();
        router.register_request(request_id.clone());

        let (tx, _rx) = mpsc::channel(100);
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .consumer_tx = Some(tx);

        // Detach
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .consumer_tx = None;
        router.requests.lock().get_mut(&request_id).unwrap().phase =
            RequestPhase::ConsumerDisconnected;

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].phase, RequestPhase::ConsumerDisconnected);
        assert!(!status[0].has_consumer);
    }

    #[tokio::test]
    async fn test_cleanup_removes_expired_terminal_requests() {
        let router = SessionStreamRouterActor::new(Some(10), Some(0)); // 0s TTL = immediate expiry
        let request_id = "test-request-9".to_string();
        router.register_request(request_id.clone());

        // Make it terminal
        let message = StreamRelayMessage::Chunk(StreamChunk::Done {
            finish_reason: querymt::chat::FinishReason::Stop,
        });
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(message);

        // Wait a tiny bit to ensure elapsed > TTL
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Cleanup should remove it
        router.cleanup_expired_requests();

        let status = router.get_status(Some(&request_id));
        assert_eq!(status.len(), 0);
    }

    #[tokio::test]
    async fn test_cleanup_preserves_non_terminal_requests() {
        let router = SessionStreamRouterActor::new(Some(10), Some(0)); // 0s TTL
        let request_id = "test-request-10".to_string();
        router.register_request(request_id.clone());

        // Keep it in AwaitingStream (non-terminal)
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Cleanup should preserve it
        router.cleanup_expired_requests();

        let status = router.get_status(Some(&request_id));
        assert_eq!(status.len(), 1);
    }

    #[tokio::test]
    async fn test_consumer_slow_sets_consumer_disconnected() {
        let router = create_test_router();
        let request_id = "test-request-11".to_string();
        router.register_request(request_id.clone());

        // Create a tiny channel that will fill up
        let (tx, _rx) = mpsc::channel(1);
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .consumer_tx = Some(tx);

        // First message should go through
        let msg1 = StreamRelayMessage::Chunk(StreamChunk::Text("first".to_string()));
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(msg1);

        // Second message should fail and set ConsumerDisconnected
        let msg2 = StreamRelayMessage::Chunk(StreamChunk::Text("second".to_string()));
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(msg2);

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].phase, RequestPhase::ConsumerDisconnected);
        assert_eq!(status[0].buffered_messages, 1); // Second message buffered
    }

    #[tokio::test]
    async fn test_get_status_returns_all_requests() {
        let router = create_test_router();

        router.register_request("req-1".to_string());
        router.register_request("req-2".to_string());
        router.register_request("req-3".to_string());

        let all_status = router.get_status(None);
        assert_eq!(all_status.len(), 3);
    }

    #[tokio::test]
    async fn test_request_phase_transitions() {
        let router = create_test_router();
        let request_id = "test-request-12".to_string();
        router.register_request(request_id.clone());

        // Initial: AwaitingStream
        assert_eq!(
            router.get_status(Some(&request_id))[0].phase,
            RequestPhase::AwaitingStream
        );

        // After first chunk: Streaming
        let msg = StreamRelayMessage::Chunk(StreamChunk::Text("chunk".to_string()));
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(msg);
        assert_eq!(
            router.get_status(Some(&request_id))[0].phase,
            RequestPhase::Streaming
        );

        // After done: Completed
        let done = StreamRelayMessage::Chunk(StreamChunk::Done {
            finish_reason: querymt::chat::FinishReason::Stop,
        });
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(done);
        assert_eq!(
            router.get_status(Some(&request_id))[0].phase,
            RequestPhase::Completed
        );
    }

    // ── Terminal delivery closes consumer ───────────────────────────────────

    /// When a terminal `Done` chunk is delivered directly to an attached
    /// consumer, the consumer sender must be dropped so the mpsc receiver
    /// sees EOF.  This mirrors the old `StreamReceiverActor` lifecycle.
    #[tokio::test]
    async fn test_terminal_chunk_direct_delivery_closes_consumer() {
        use querymt::chat::{FinishReason, StreamChunk};
        use std::time::Duration;

        let router_ref = SessionStreamRouterActor::spawn(SessionStreamRouterActor::new(None, None));
        let request_id = "req-terminal-chunk".to_string();
        let (tx, mut rx) = mpsc::channel(8);

        // Attach consumer
        router_ref
            .ask(AttachStreamConsumer {
                request_id: request_id.clone(),
                consumer_tx: tx,
            })
            .await
            .unwrap();

        // Send a terminal Done chunk
        router_ref
            .tell(RoutedStreamRelayMessage {
                request_id: request_id.clone(),
                message: StreamRelayMessage::Chunk(StreamChunk::Done {
                    finish_reason: FinishReason::Stop,
                }),
            })
            .send()
            .await;

        // Consumer should receive the Done message
        let first = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("should receive terminal message")
            .expect("should have a message");
        assert!(matches!(
            first,
            StreamRelayMessage::Chunk(StreamChunk::Done {
                finish_reason: FinishReason::Stop
            })
        ));

        // Channel should be closed (EOF) because the router dropped its sender
        let closed = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("recv should not hang");
        assert!(
            closed.is_none(),
            "consumer channel should be closed after terminal delivery"
        );
    }

    /// When a terminal `ChunkBatch` (containing `Done`) is delivered to an
    /// attached consumer, the consumer sender must be dropped.
    #[tokio::test]
    async fn test_terminal_chunk_batch_direct_delivery_closes_consumer() {
        use querymt::chat::{FinishReason, StreamChunk};
        use std::time::Duration;

        let router_ref = SessionStreamRouterActor::spawn(SessionStreamRouterActor::new(None, None));
        let request_id = "req-terminal-batch".to_string();
        let (tx, mut rx) = mpsc::channel(8);

        router_ref
            .ask(AttachStreamConsumer {
                request_id: request_id.clone(),
                consumer_tx: tx,
            })
            .await
            .unwrap();

        // Send a ChunkBatch containing a text chunk + Done(ToolCalls)
        router_ref
            .tell(RoutedStreamRelayMessage {
                request_id: request_id.clone(),
                message: StreamRelayMessage::ChunkBatch(vec![
                    StreamChunk::Text("thinking...".to_string().into()),
                    StreamChunk::Done {
                        finish_reason: FinishReason::ToolCalls,
                    },
                ]),
            })
            .send()
            .await;

        // Consumer should receive the batch
        let first = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("should receive batch")
            .expect("should have a message");
        assert!(matches!(
            first,
            StreamRelayMessage::ChunkBatch(chunks) if chunks.len() == 2
        ));

        // Channel should be closed after terminal batch delivery
        let closed = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("recv should not hang");
        assert!(
            closed.is_none(),
            "consumer channel should be closed after terminal ChunkBatch delivery"
        );
    }

    /// A ChunkBatch containing Done should set phase to Completed, not Failed.
    #[tokio::test]
    async fn test_terminal_chunk_batch_sets_completed_not_failed() {
        use querymt::chat::{FinishReason, StreamChunk};

        let router = create_test_router();
        let request_id = "req-batch-phase".to_string();
        router.register_request(request_id.clone());

        let (tx, mut rx) = mpsc::channel(8);
        {
            let mut requests = router.requests.lock();
            let req = requests.get_mut(&request_id).unwrap();
            req.consumer_tx = Some(tx);
        }

        // Deliver a ChunkBatch containing Done
        let msg = StreamRelayMessage::ChunkBatch(vec![
            StreamChunk::Text("hello".to_string().into()),
            StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            },
        ]);
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .unwrap()
            .deliver_or_buffer(msg);

        let status = router.get_status(Some(&request_id));
        assert_eq!(
            status[0].phase,
            RequestPhase::Completed,
            "ChunkBatch with Done should set phase to Completed, not Failed"
        );

        // Consumer sender should have been dropped
        assert!(
            router
                .requests
                .lock()
                .get(&request_id)
                .unwrap()
                .consumer_tx
                .is_none(),
            "consumer_tx should be None after terminal delivery"
        );

        // Drain the received message
        let _ = rx.try_recv();
        // Receiver should see EOF
        assert!(rx.try_recv().is_err(), "channel should be closed");
    }

    /// When buffered messages contain a terminal message and a consumer
    /// re-attaches, the replayed terminal should close the consumer.
    #[tokio::test]
    async fn test_terminal_buffered_replay_closes_consumer() {
        use querymt::chat::{FinishReason, StreamChunk};
        use std::time::Duration;

        let router_ref = SessionStreamRouterActor::spawn(SessionStreamRouterActor::new(None, None));
        let request_id = "req-buffered-terminal".to_string();

        // Send messages without a consumer — they will be buffered
        router_ref
            .tell(RoutedStreamRelayMessage {
                request_id: request_id.clone(),
                message: StreamRelayMessage::Chunk(StreamChunk::Text("hello".to_string().into())),
            })
            .send()
            .await;

        router_ref
            .tell(RoutedStreamRelayMessage {
                request_id: request_id.clone(),
                message: StreamRelayMessage::Chunk(StreamChunk::Done {
                    finish_reason: FinishReason::Stop,
                }),
            })
            .send()
            .await;

        // Now attach a consumer — buffered messages should be replayed
        let (tx, mut rx) = mpsc::channel(8);
        router_ref
            .ask(AttachStreamConsumer {
                request_id: request_id.clone(),
                consumer_tx: tx,
            })
            .await
            .unwrap();

        // Should receive the text chunk
        let first = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("should receive text chunk")
            .expect("should have a message");
        assert!(matches!(
            first,
            StreamRelayMessage::Chunk(StreamChunk::Text(_))
        ));

        // Should receive the Done chunk
        let second = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("should receive Done")
            .expect("should have a message");
        assert!(matches!(
            second,
            StreamRelayMessage::Chunk(StreamChunk::Done { .. })
        ));

        // Channel should be closed after replaying terminal
        let closed = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("recv should not hang");
        assert!(
            closed.is_none(),
            "consumer channel should be closed after replaying terminal buffered messages"
        );
    }
}
