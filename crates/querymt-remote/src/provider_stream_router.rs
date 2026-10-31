//! Stable router for remote provider stream relay messages.
//!
//! This actor buffers routed provider stream messages per request, supports
//! consumer re-attachment, and preserves a stable remote actor ID during the
//! migration from the agent-owned router.

use crate::provider_protocol::{StreamRelayMessage, relay_message_is_terminal};
use crate::stream_router_protocol::{
    GetRouterStatus, RequestPhase, RoutedRequestStatus, RoutedStreamRelayMessage,
    terminal_request_phase,
};
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;

const DEFAULT_REPLAY_BUFFER_SIZE: usize = 1000;
const DEFAULT_REQUEST_TTL_SECS: u64 = 300;
const DEFAULT_GAP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_MAX_HELD: usize = 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RouterError {
    #[error("router invariant violated: {0}")]
    Invariant(String),
}

#[derive(Debug)]
pub struct AttachStreamConsumer {
    pub request_id: String,
    pub consumer_tx: mpsc::UnboundedSender<StreamRelayMessage>,
}

/// Local message scheduled when a sequence gap opens; fails the request if the
/// gap is still unfilled after the router's gap timeout.
#[derive(Debug, Clone)]
pub struct CheckReorderGap {
    pub request_id: String,
}

#[derive(Debug, Clone)]
pub struct RegisterRequest {
    pub request_id: String,
}

#[derive(Debug, Clone)]
pub struct DetachStreamConsumer {
    pub request_id: String,
}

/// Outcome of admitting a sequenced message.
#[derive(Debug)]
enum Admitted {
    /// The message and every contiguous held successor, in order.
    Ready(Vec<StreamRelayMessage>),
    /// The message arrived early and is held until the gap fills.
    Held,
    /// Already delivered or already held.
    Duplicate,
}

/// Per-request reorder buffer. Pure: no I/O, no timers.
#[derive(Debug, Default)]
struct ReorderBuffer {
    /// Next sequence number to deliver.
    next: u64,
    held: BTreeMap<u64, StreamRelayMessage>,
    /// When the current gap opened or last made progress.
    stalled_since: Option<Instant>,
}

impl ReorderBuffer {
    fn admit(&mut self, seq: u64, message: StreamRelayMessage) -> Admitted {
        if seq < self.next || self.held.contains_key(&seq) {
            return Admitted::Duplicate;
        }
        if seq > self.next {
            self.held.insert(seq, message);
            self.stalled_since.get_or_insert_with(Instant::now);
            return Admitted::Held;
        }
        let mut ready = vec![message];
        self.next += 1;
        while let Some(successor) = self.held.remove(&self.next) {
            ready.push(successor);
            self.next += 1;
        }
        self.stalled_since = if self.held.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
        Admitted::Ready(ready)
    }
}

enum GapCheck {
    /// Nothing held, request finished, or the gap already closed.
    Idle,
    /// The gap outlived the timeout and the request was failed.
    Failed,
    /// The stall is younger than the timeout; check again after this long.
    Recheck(Duration),
}

struct RoutedRequest {
    request_id: String,
    phase: RequestPhase,
    created_at: Instant,
    last_message_at: Instant,
    consumer_tx: Option<mpsc::UnboundedSender<StreamRelayMessage>>,
    replay_buffer: VecDeque<StreamRelayMessage>,
    replay_buffer_size: usize,
    reorder: ReorderBuffer,
    max_held: usize,
}

impl RoutedRequest {
    fn new(request_id: String, replay_buffer_size: usize, max_held: usize) -> Self {
        let now = Instant::now();
        Self {
            request_id,
            phase: RequestPhase::AwaitingStream,
            created_at: now,
            last_message_at: now,
            consumer_tx: None,
            replay_buffer: VecDeque::with_capacity(replay_buffer_size.min(64)),
            replay_buffer_size,
            reorder: ReorderBuffer::default(),
            max_held,
        }
    }

    /// Route a relay message. Unnumbered messages are delivered immediately;
    /// numbered ones are delivered in host order. Returns `true` when a gap
    /// check should be scheduled (a gap opened or its stall clock restarted).
    fn route(&mut self, seq: Option<u64>, message: StreamRelayMessage) -> bool {
        let Some(seq) = seq else {
            self.deliver_or_buffer(message);
            return false;
        };
        if self.is_terminal() {
            self.last_message_at = Instant::now();
            tracing::debug!(
                target: "querymt_remote::provider::router",
                request_id = %self.request_id,
                seq,
                phase = %self.phase,
                "discarded sequenced relay message after terminal"
            );
            return false;
        }
        let stalled_before = self.reorder.stalled_since;
        match self.reorder.admit(seq, message) {
            Admitted::Ready(messages) => {
                for message in messages {
                    self.deliver_or_buffer(message);
                }
            }
            Admitted::Held => {
                self.last_message_at = Instant::now();
                tracing::debug!(
                    target: "querymt_remote::provider::router",
                    request_id = %self.request_id,
                    seq,
                    next = self.reorder.next,
                    held = self.reorder.held.len(),
                    "held out-of-order relay message"
                );
                if self.reorder.held.len() > self.max_held {
                    self.fail_gap();
                    return false;
                }
            }
            Admitted::Duplicate => {
                self.last_message_at = Instant::now();
                tracing::debug!(
                    target: "querymt_remote::provider::router",
                    request_id = %self.request_id,
                    seq,
                    next = self.reorder.next,
                    "discarded duplicate relay message"
                );
            }
        }
        let stalled_after = self.reorder.stalled_since;
        stalled_after.is_some() && stalled_after != stalled_before
    }

    /// End the stream with a retryable transport failure naming the first
    /// missing sequence number. Held messages are discarded.
    fn fail_gap(&mut self) {
        let missing = self.reorder.next;
        let held = self.reorder.held.len();
        self.reorder.held.clear();
        self.reorder.stalled_since = None;
        tracing::warn!(
            target: "querymt_remote::provider::router",
            request_id = %self.request_id,
            missing,
            held,
            "relay sequence gap; failing stream"
        );
        self.deliver_or_buffer(StreamRelayMessage::TransportFailed {
            error: querymt::error::LLMError::Transport {
                kind: querymt::error::TransportErrorKind::Other,
                message: format!("relay sequence gap: missing seq {missing} (held {held})"),
            }
            .to_payload(),
        });
    }

    fn check_gap(&mut self, gap_timeout: Duration) -> GapCheck {
        if self.reorder.held.is_empty() || self.is_terminal() {
            return GapCheck::Idle;
        }
        let Some(since) = self.reorder.stalled_since else {
            return GapCheck::Idle;
        };
        let elapsed = since.elapsed();
        if elapsed >= gap_timeout {
            self.fail_gap();
            GapCheck::Failed
        } else {
            GapCheck::Recheck(gap_timeout - elapsed)
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

    fn deliver_or_buffer(&mut self, message: StreamRelayMessage) {
        self.last_message_at = Instant::now();

        let message_type = match &message {
            StreamRelayMessage::Chunk(_) => "chunk",
            StreamRelayMessage::ChunkBatch(_) => "chunk_batch",
            StreamRelayMessage::Heartbeat { .. } => "heartbeat",
            StreamRelayMessage::ProviderError { .. } => "provider_error",
            StreamRelayMessage::TransportDisconnected { .. } => "transport_disconnected",
            StreamRelayMessage::TransportReconnected { .. } => "transport_reconnected",
            StreamRelayMessage::TransportFailed { .. } => "transport_failed",
        };
        let is_terminal = relay_message_is_terminal(&message);
        let terminal_phase = terminal_request_phase(&message);

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

        let message = if let Some(tx) = &self.consumer_tx {
            match tx.send(message) {
                Ok(()) => {
                    tracing::debug!(
                        target: "querymt_remote::provider::router",
                        request_id = %self.request_id,
                        message_type,
                        is_terminal,
                        phase = %self.phase,
                        buffered = self.replay_buffer.len(),
                        "router delivered message to stream consumer"
                    );
                    if is_terminal {
                        if let Some(phase) = terminal_phase {
                            self.phase = phase;
                        }
                        tracing::info!(
                            target: "querymt_remote::provider::router",
                            request_id = %self.request_id,
                            message_type,
                            phase = %self.phase,
                            "router delivered terminal message to stream consumer"
                        );
                        self.consumer_tx = None;
                    }
                    return;
                }
                Err(mpsc::error::SendError(msg)) => {
                    tracing::warn!(
                        target: "querymt_remote::provider::router",
                        request_id = %self.request_id,
                        message_type,
                        is_terminal,
                        phase = %self.phase,
                        buffered = self.replay_buffer.len(),
                        "router consumer channel closed; buffering message"
                    );
                    self.consumer_tx = None;
                    self.phase = RequestPhase::ConsumerDisconnected;
                    msg
                }
            }
        } else {
            tracing::debug!(
                target: "querymt_remote::provider::router",
                request_id = %self.request_id,
                message_type,
                is_terminal,
                phase = %self.phase,
                buffered = self.replay_buffer.len(),
                "router has no active consumer; buffering message"
            );
            message
        };

        if self.replay_buffer.len() >= self.replay_buffer_size {
            self.replay_buffer.pop_front();
        }
        self.replay_buffer.push_back(message);

        if relay_message_is_terminal(
            self.replay_buffer
                .back()
                .expect("buffer contains pushed message"),
        ) && let Some(phase) = terminal_phase
        {
            self.phase = phase;
            tracing::info!(
                target: "querymt_remote::provider::router",
                request_id = %self.request_id,
                message_type,
                phase = %self.phase,
                buffered = self.replay_buffer.len(),
                "router buffered terminal message"
            );
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self.phase,
            RequestPhase::Completed | RequestPhase::Failed | RequestPhase::Cancelled
        )
    }
}

#[derive(Actor)]
pub struct ProviderStreamRouterActor {
    requests: Arc<Mutex<HashMap<String, RoutedRequest>>>,
    replay_buffer_size: usize,
    request_ttl: Duration,
    gap_timeout: Duration,
    max_held: usize,
}

impl ProviderStreamRouterActor {
    pub fn new(replay_buffer_size: Option<usize>, request_ttl_secs: Option<u64>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(HashMap::new())),
            replay_buffer_size: replay_buffer_size.unwrap_or(DEFAULT_REPLAY_BUFFER_SIZE),
            request_ttl: Duration::from_secs(request_ttl_secs.unwrap_or(DEFAULT_REQUEST_TTL_SECS)),
            gap_timeout: DEFAULT_GAP_TIMEOUT,
            max_held: DEFAULT_MAX_HELD,
        }
    }

    /// Override the sequence-gap wait (default 15 s) and the held-message cap
    /// (default 1024). Existing `new` callers keep the defaults.
    pub fn with_gap_limits(mut self, gap_timeout: Duration, max_held: usize) -> Self {
        self.gap_timeout = gap_timeout;
        self.max_held = max_held;
        self
    }

    pub fn register_request(&self, request_id: String) -> Result<(), RouterError> {
        let mut requests = self.requests.lock();
        if requests.contains_key(&request_id) {
            return Err(RouterError::Invariant(format!(
                "request {} already registered",
                request_id
            )));
        }
        requests.insert(
            request_id.clone(),
            RoutedRequest::new(request_id, self.replay_buffer_size, self.max_held),
        );
        Ok(())
    }

    fn cleanup_expired_requests(&self) {
        let mut requests = self.requests.lock();
        requests.retain(|_, req| {
            if req.is_terminal() {
                return req.last_message_at.elapsed() < self.request_ttl;
            }
            if req.consumer_tx.is_none() && req.last_message_at.elapsed() > self.request_ttl * 2 {
                tracing::warn!(
                    target: "querymt_remote::provider::router",
                    request_id = %req.request_id,
                    phase = %req.phase,
                    idle_secs = req.last_message_at.elapsed().as_secs(),
                    "pruning abandoned non-terminal request"
                );
                return false;
            }
            true
        });
    }

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

impl Message<RoutedStreamRelayMessage> for ProviderStreamRouterActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: RoutedStreamRelayMessage,
        ctx: &mut Context<Self, Self::Reply>,
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

        let mut requests = self.requests.lock();
        let Some(request) = requests.get_mut(&msg.request_id) else {
            tracing::warn!(
                target: "querymt_remote::provider::router",
                request_id = %msg.request_id,
                message_type,
                "dropped relay message for unknown request id"
            );
            return;
        };

        let schedule_check = request.route(msg.seq, msg.message);
        tracing::trace!(
            target: "querymt_remote::provider::router",
            request_id = %msg.request_id,
            phase = %request.phase,
            has_consumer = request.consumer_tx.is_some(),
            buffered = request.replay_buffer.len(),
            "routed message delivered or buffered"
        );
        drop(requests);
        if schedule_check {
            schedule_gap_check(
                ctx.actor_ref().clone(),
                msg.request_id.clone(),
                self.gap_timeout,
            );
        }
        self.cleanup_expired_requests();
    }
}

/// Send a `CheckReorderGap` to the router after `delay`, without keeping the
/// actor alive while it waits.
fn schedule_gap_check(
    actor_ref: kameo::actor::ActorRef<ProviderStreamRouterActor>,
    request_id: String,
    delay: Duration,
) {
    let weak = actor_ref.downgrade();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        if let Some(actor_ref) = weak.upgrade() {
            let _ = actor_ref.tell(CheckReorderGap { request_id }).await;
        }
    });
}

impl Message<CheckReorderGap> for ProviderStreamRouterActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: CheckReorderGap,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let outcome = {
            let mut requests = self.requests.lock();
            let Some(request) = requests.get_mut(&msg.request_id) else {
                return;
            };
            request.check_gap(self.gap_timeout)
        };
        if let GapCheck::Recheck(remaining) = outcome {
            schedule_gap_check(ctx.actor_ref().clone(), msg.request_id, remaining);
        }
    }
}

impl Message<AttachStreamConsumer> for ProviderStreamRouterActor {
    type Reply = Result<(), RouterError>;

    async fn handle(
        &mut self,
        msg: AttachStreamConsumer,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let request_id = msg.request_id.clone();
        let consumer_tx = msg.consumer_tx;
        self.cleanup_expired_requests();

        {
            let requests = self.requests.lock();
            if !requests.contains_key(&request_id) {
                return Err(RouterError::Invariant(format!(
                    "request {} not pre-registered",
                    request_id
                )));
            }
        }

        let mut requests = self.requests.lock();
        let Some(request) = requests.get_mut(&request_id) else {
            return Err(RouterError::Invariant(format!(
                "request {} disappeared before attach",
                request_id
            )));
        };

        request.consumer_tx = Some(consumer_tx.clone());
        let terminal_state = request.is_terminal();
        if request.phase == RequestPhase::ConsumerDisconnected {
            request.phase = if request.replay_buffer.is_empty() {
                RequestPhase::AwaitingStream
            } else {
                RequestPhase::Streaming
            };
        }

        let mut terminal_replayed = false;
        while let Some(message) = request.replay_buffer.pop_front() {
            terminal_replayed = relay_message_is_terminal(&message);
            match consumer_tx.send(message) {
                Ok(()) => {}
                Err(mpsc::error::SendError(msg)) => {
                    request.replay_buffer.push_front(msg);
                    request.consumer_tx = None;
                    request.phase = RequestPhase::ConsumerDisconnected;
                    break;
                }
            }
        }

        if request.replay_buffer.is_empty() && !terminal_state {
            request.phase = RequestPhase::Streaming;
        }
        if terminal_replayed && request.replay_buffer.is_empty() {
            request.consumer_tx = None;
        }

        Ok(())
    }
}

impl Message<DetachStreamConsumer> for ProviderStreamRouterActor {
    type Reply = ();

    async fn handle(
        &mut self,
        msg: DetachStreamConsumer,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.cleanup_expired_requests();
        let mut requests = self.requests.lock();
        if let Some(request) = requests.get_mut(&msg.request_id) {
            request.consumer_tx = None;
            if !request.is_terminal() {
                request.phase = RequestPhase::ConsumerDisconnected;
            }
        }
    }
}

impl Message<RegisterRequest> for ProviderStreamRouterActor {
    type Reply = Result<(), RouterError>;

    async fn handle(
        &mut self,
        msg: RegisterRequest,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.register_request(msg.request_id)
    }
}

impl Message<GetRouterStatus> for ProviderStreamRouterActor {
    type Reply = Vec<RoutedRequestStatus>;

    async fn handle(
        &mut self,
        msg: GetRouterStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.get_status(msg.request_id.as_deref())
    }
}

impl kameo::remote::RemoteActor for ProviderStreamRouterActor {
    // Keep the old ID stable during migration.
    const REMOTE_ID: &'static str = "querymt::SessionStreamRouterActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static PROVIDER_STREAM_ROUTER_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <ProviderStreamRouterActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<ProviderStreamRouterActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<ProviderStreamRouterActor>(
                actor_id, sibling_id,
            ))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<ProviderStreamRouterActor>(
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

remote_router_msg_impl!(
    ProviderStreamRouterActor,
    RoutedStreamRelayMessage,
    "querymt::RoutedStreamRelayMessage",
    REG_ROUTED_STREAM_RELAY_MESSAGE
);
remote_router_msg_impl!(
    ProviderStreamRouterActor,
    GetRouterStatus,
    "querymt::GetRouterStatus",
    REG_GET_ROUTER_STATUS
);

#[cfg(test)]
mod tests {
    use super::*;
    use kameo::actor::Spawn;
    use querymt::chat::StreamChunk;

    fn create_test_router() -> ProviderStreamRouterActor {
        ProviderStreamRouterActor::new(Some(10), Some(60))
    }

    #[tokio::test]
    async fn register_request_creates_entry() {
        let router = create_test_router();
        let request_id = "test-request-1".to_string();
        router
            .register_request(request_id.clone())
            .expect("register request");
        let status = router.get_status(Some(&request_id));
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].request_id, request_id);
        assert_eq!(status[0].phase, RequestPhase::AwaitingStream);
    }

    #[tokio::test]
    async fn attach_requires_preregistration() {
        let router_ref = ProviderStreamRouterActor::spawn(create_test_router());
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = router_ref
            .ask(AttachStreamConsumer {
                request_id: "missing".to_string(),
                consumer_tx: tx,
            })
            .await
            .expect_err("attach should fail");
        assert!(matches!(
            err,
            kameo::error::SendError::HandlerError(RouterError::Invariant(_))
        ));
    }

    #[tokio::test]
    async fn terminal_replay_closes_consumer() {
        let router = create_test_router();
        let request_id = "test-request-2".to_string();
        router
            .register_request(request_id.clone())
            .expect("register request");
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .expect("request exists")
            .deliver_or_buffer(StreamRelayMessage::Chunk(StreamChunk::Done {
                finish_reason: querymt::chat::FinishReason::Stop,
            }));

        let router_ref = ProviderStreamRouterActor::spawn(router);
        let (tx, mut rx) = mpsc::unbounded_channel();
        router_ref
            .ask(AttachStreamConsumer {
                request_id,
                consumer_tx: tx,
            })
            .await
            .expect("attach should succeed");

        assert!(matches!(
            rx.recv().await,
            Some(StreamRelayMessage::Chunk(StreamChunk::Done { .. }))
        ));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn buffer_overflow_evicts_oldest_message() {
        let router = ProviderStreamRouterActor::new(Some(3), Some(60));
        let request_id = "test-request-3".to_string();
        router
            .register_request(request_id.clone())
            .expect("register request");

        for i in 0..4 {
            router
                .requests
                .lock()
                .get_mut(&request_id)
                .expect("request exists")
                .deliver_or_buffer(StreamRelayMessage::Chunk(StreamChunk::Text(format!(
                    "msg-{i}"
                ))));
        }

        let request = router.requests.lock();
        let request = request.get(&request_id).expect("request exists");
        assert_eq!(request.replay_buffer.len(), 3);
        assert!(matches!(
            request.replay_buffer.front(),
            Some(StreamRelayMessage::Chunk(StreamChunk::Text(text))) if text == "msg-1"
        ));
    }

    #[tokio::test]
    async fn detach_sets_consumer_disconnected_for_non_terminal_requests() {
        let router_ref = ProviderStreamRouterActor::spawn(create_test_router());
        let request_id = "test-request-4".to_string();
        router_ref
            .ask(RegisterRequest {
                request_id: request_id.clone(),
            })
            .await
            .expect("register should succeed");
        let (tx, _rx) = mpsc::unbounded_channel();
        router_ref
            .ask(AttachStreamConsumer {
                request_id: request_id.clone(),
                consumer_tx: tx,
            })
            .await
            .expect("attach should succeed");

        router_ref
            .tell(DetachStreamConsumer {
                request_id: request_id.clone(),
            })
            .await
            .expect("detach");

        let statuses = router_ref
            .ask(GetRouterStatus {
                request_id: Some(request_id),
            })
            .await
            .expect("status should succeed");
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].phase, RequestPhase::ConsumerDisconnected);
        assert!(!statuses[0].has_consumer);
    }

    #[tokio::test]
    async fn cleanup_removes_expired_terminal_requests() {
        let router = ProviderStreamRouterActor::new(Some(10), Some(1));
        let request_id = "test-request-5".to_string();
        router
            .register_request(request_id.clone())
            .expect("register request");
        {
            let mut requests = router.requests.lock();
            let request = requests.get_mut(&request_id).expect("request exists");
            request.phase = RequestPhase::Completed;
            request.last_message_at = Instant::now() - Duration::from_secs(2);
        }

        router.cleanup_expired_requests();
        assert!(router.get_status(Some(&request_id)).is_empty());
    }

    #[tokio::test]
    async fn direct_terminal_delivery_closes_consumer_and_marks_completed() {
        let router = create_test_router();
        let request_id = "test-request-6".to_string();
        router
            .register_request(request_id.clone())
            .expect("register request");
        let (tx, mut rx) = mpsc::unbounded_channel();
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .expect("request exists")
            .consumer_tx = Some(tx);

        router
            .requests
            .lock()
            .get_mut(&request_id)
            .expect("request exists")
            .deliver_or_buffer(StreamRelayMessage::Chunk(StreamChunk::Done {
                finish_reason: querymt::chat::FinishReason::Stop,
            }));

        assert!(matches!(
            rx.recv().await,
            Some(StreamRelayMessage::Chunk(StreamChunk::Done { .. }))
        ));
        assert!(rx.recv().await.is_none());
        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].phase, RequestPhase::Completed);
        assert!(!status[0].has_consumer);
    }

    #[tokio::test]
    async fn get_status_without_request_id_returns_all_requests() {
        let router = create_test_router();
        router
            .register_request("req-a".to_string())
            .expect("register req-a");
        router
            .register_request("req-b".to_string())
            .expect("register req-b");

        let mut ids = router
            .get_status(None)
            .into_iter()
            .map(|status| status.request_id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, vec!["req-a".to_string(), "req-b".to_string()]);
    }

    #[tokio::test]
    async fn cleanup_preserves_non_terminal_requests() {
        let router = create_test_router();
        let request_id = "test-request-7".to_string();
        router
            .register_request(request_id.clone())
            .expect("register request");
        {
            let mut requests = router.requests.lock();
            let request = requests.get_mut(&request_id).expect("request exists");
            request.last_message_at = Instant::now() - Duration::from_secs(1);
        }

        router.cleanup_expired_requests();

        let status = router.get_status(Some(&request_id));
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].phase, RequestPhase::AwaitingStream);
    }

    #[tokio::test]
    async fn slow_consumer_does_not_lose_or_reorder_messages() {
        let router = create_test_router();
        let request_id = "test-request-8".to_string();
        let mut rx = router_with_consumer(&router, &request_id);

        for i in 0..500u64 {
            deliver(
                &router,
                &request_id,
                i,
                StreamRelayMessage::Chunk(StreamChunk::Text(format!("msg-{i}"))),
            );
        }

        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].phase, RequestPhase::Streaming);
        assert_eq!(status[0].buffered_messages, 0);
        for i in 0..500 {
            match rx.try_recv() {
                Ok(StreamRelayMessage::Chunk(StreamChunk::Text(text))) => {
                    assert_eq!(text, format!("msg-{i}"));
                }
                other => panic!("message {i}: unexpected {other:?}"),
            }
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn request_phase_transitions_from_awaiting_to_streaming_to_completed() {
        let router = create_test_router();
        let request_id = "test-request-9".to_string();
        router
            .register_request(request_id.clone())
            .expect("register request");

        assert_eq!(
            router.get_status(Some(&request_id))[0].phase,
            RequestPhase::AwaitingStream
        );

        router
            .requests
            .lock()
            .get_mut(&request_id)
            .expect("request exists")
            .deliver_or_buffer(StreamRelayMessage::Chunk(StreamChunk::Text(
                "chunk".to_string(),
            )));
        assert_eq!(
            router.get_status(Some(&request_id))[0].phase,
            RequestPhase::Streaming
        );

        router
            .requests
            .lock()
            .get_mut(&request_id)
            .expect("request exists")
            .deliver_or_buffer(StreamRelayMessage::Chunk(StreamChunk::Done {
                finish_reason: querymt::chat::FinishReason::Stop,
            }));
        assert_eq!(
            router.get_status(Some(&request_id))[0].phase,
            RequestPhase::Completed
        );
    }

    // ---- ordered relay helpers (fixture-driven) ----

    use querymt::chat::{ChatOutput, ChatStreamAccumulator};

    fn fixture_chunks() -> Vec<StreamChunk> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../providers/anthropic/tests/fixtures/structured_tool_turn.json"
        );
        let raw = std::fs::read_to_string(path).expect("read fixture");
        serde_json::from_str(&raw).expect("parse fixture")
    }

    /// Group chunks into relay messages the way the host does.
    fn host_batches(chunks: &[StreamChunk], size: usize) -> Vec<StreamRelayMessage> {
        chunks
            .chunks(size)
            .map(|group| {
                if group.len() == 1 {
                    StreamRelayMessage::Chunk(group[0].clone())
                } else {
                    StreamRelayMessage::ChunkBatch(group.to_vec())
                }
            })
            .collect()
    }

    fn accumulate_direct(chunks: &[StreamChunk]) -> ChatOutput {
        let mut acc = ChatStreamAccumulator::new();
        for chunk in chunks {
            acc.push(chunk).expect("direct accumulation");
        }
        acc.finish_success().expect("direct finish")
    }

    /// Drain everything currently readable from the consumer and accumulate it.
    fn accumulate_consumer(
        rx: &mut mpsc::UnboundedReceiver<StreamRelayMessage>,
    ) -> Result<ChatOutput, String> {
        let mut acc = ChatStreamAccumulator::new();
        while let Ok(message) = rx.try_recv() {
            match message {
                StreamRelayMessage::Chunk(chunk) => {
                    acc.push(&chunk).map_err(|e| e.to_string())?;
                }
                StreamRelayMessage::ChunkBatch(chunks) => {
                    for chunk in &chunks {
                        acc.push(chunk).map_err(|e| e.to_string())?;
                    }
                }
                StreamRelayMessage::Heartbeat { .. }
                | StreamRelayMessage::TransportDisconnected { .. }
                | StreamRelayMessage::TransportReconnected { .. } => {}
                other => return Err(format!("unexpected message: {other:?}")),
            }
        }
        acc.finish_success().map_err(|e| e.to_string())
    }

    fn router_with_consumer(
        router: &ProviderStreamRouterActor,
        request_id: &str,
    ) -> mpsc::UnboundedReceiver<StreamRelayMessage> {
        router
            .register_request(request_id.to_string())
            .expect("register request");
        let (tx, rx) = mpsc::unbounded_channel();
        router
            .requests
            .lock()
            .get_mut(request_id)
            .expect("request exists")
            .consumer_tx = Some(tx);
        rx
    }

    /// Deliver a host-numbered message to the request the way the router
    /// handler does.
    fn deliver(
        router: &ProviderStreamRouterActor,
        request_id: &str,
        seq: u64,
        msg: StreamRelayMessage,
    ) {
        router
            .requests
            .lock()
            .get_mut(request_id)
            .expect("request exists")
            .route(Some(seq), msg);
    }

    fn deliver_all(
        router: &ProviderStreamRouterActor,
        request_id: &str,
        batches: &[StreamRelayMessage],
        order: &[usize],
    ) {
        for &index in order {
            deliver(router, request_id, index as u64, batches[index].clone());
        }
    }

    fn fixture_router() -> ProviderStreamRouterActor {
        ProviderStreamRouterActor::new(Some(10), Some(60))
    }

    #[tokio::test]
    async fn in_order_delivery_matches_direct_accumulation() {
        let chunks = fixture_chunks();
        let expected = accumulate_direct(&chunks);
        let batches = host_batches(&chunks, 16);
        assert!(batches.len() >= 2);

        let router = fixture_router();
        let mut rx = router_with_consumer(&router, "r");
        let order: Vec<usize> = (0..batches.len()).collect();
        deliver_all(&router, "r", &batches, &order);

        assert_eq!(accumulate_consumer(&mut rx).expect("accumulate"), expected);
    }

    #[tokio::test]
    async fn swapped_adjacent_batches_are_reordered() {
        let chunks = fixture_chunks();
        let expected = accumulate_direct(&chunks);
        let batches = host_batches(&chunks, 4);
        assert!(batches.len() >= 4);

        let router = fixture_router();
        let mut rx = router_with_consumer(&router, "r");
        let mut order: Vec<usize> = (0..batches.len()).collect();
        order.swap(1, 2);
        deliver_all(&router, "r", &batches, &order);

        assert_eq!(accumulate_consumer(&mut rx).expect("accumulate"), expected);
    }

    #[tokio::test]
    async fn terminal_batch_is_released_after_the_batch_before_it() {
        let chunks = fixture_chunks();
        let expected = accumulate_direct(&chunks);
        let batches = host_batches(&chunks, 4);
        let last = batches.len() - 1;

        let router = fixture_router();
        let mut rx = router_with_consumer(&router, "r");
        let mut order: Vec<usize> = (0..batches.len()).collect();
        order.swap(last - 1, last);
        deliver_all(&router, "r", &batches, &order);

        assert_eq!(accumulate_consumer(&mut rx).expect("accumulate"), expected);
    }

    // ---- ReorderBuffer ----

    fn text(s: &str) -> StreamRelayMessage {
        StreamRelayMessage::Chunk(StreamChunk::Text(s.to_string()))
    }

    fn ready_texts(admitted: Admitted) -> Vec<String> {
        match admitted {
            Admitted::Ready(messages) => messages
                .into_iter()
                .map(|m| match m {
                    StreamRelayMessage::Chunk(StreamChunk::Text(t)) => t,
                    other => panic!("unexpected {other:?}"),
                })
                .collect(),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn reorder_buffer_releases_in_order_messages_immediately() {
        let mut buf = ReorderBuffer::default();
        assert_eq!(ready_texts(buf.admit(0, text("a"))), vec!["a"]);
        assert_eq!(ready_texts(buf.admit(1, text("b"))), vec!["b"]);
        assert!(buf.stalled_since.is_none());
        assert_eq!(buf.next, 2);
    }

    #[test]
    fn reorder_buffer_holds_early_message_and_releases_contiguous_successors() {
        let mut buf = ReorderBuffer::default();
        assert!(matches!(buf.admit(2, text("c")), Admitted::Held));
        assert!(matches!(buf.admit(1, text("b")), Admitted::Held));
        assert!(buf.stalled_since.is_some());
        assert_eq!(ready_texts(buf.admit(0, text("a"))), vec!["a", "b", "c"]);
        assert!(buf.held.is_empty());
        assert!(buf.stalled_since.is_none());
    }

    #[test]
    fn reorder_buffer_discards_delivered_and_held_duplicates() {
        let mut buf = ReorderBuffer::default();
        ready_texts(buf.admit(0, text("a")));
        assert!(matches!(buf.admit(0, text("a")), Admitted::Duplicate));
        assert!(matches!(buf.admit(2, text("c")), Admitted::Held));
        assert!(matches!(buf.admit(2, text("c")), Admitted::Duplicate));
        assert_eq!(buf.held.len(), 1);
    }

    #[test]
    fn reorder_buffer_stall_clock_resets_on_progress_while_gap_remains() {
        let mut buf = ReorderBuffer::default();
        assert!(matches!(buf.admit(1, text("b")), Admitted::Held));
        assert!(matches!(buf.admit(3, text("d")), Admitted::Held));
        let first_stall = buf.stalled_since.expect("stall started");
        std::thread::sleep(Duration::from_millis(5));
        // Filling 0 releases 0 and 1, but 2 is still missing.
        assert_eq!(ready_texts(buf.admit(0, text("a"))), vec!["a", "b"]);
        let reset = buf.stalled_since.expect("gap remains");
        assert!(reset > first_stall);
        assert_eq!(ready_texts(buf.admit(2, text("c"))), vec!["c", "d"]);
        assert!(buf.stalled_since.is_none());
    }

    // ---- shuffled delivery ----

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 33
        }
    }

    #[tokio::test]
    async fn shuffled_delivery_with_duplicates_always_matches_direct_accumulation() {
        let chunks = fixture_chunks();
        let expected = accumulate_direct(&chunks);
        let batches = host_batches(&chunks, 2);
        assert!(batches.len() >= 6);

        for seed in 0..250u64 {
            let mut rng = Lcg(seed.wrapping_add(1));
            let mut order: Vec<usize> = (0..batches.len()).collect();
            for i in (1..order.len()).rev() {
                order.swap(i, (rng.next() as usize) % (i + 1));
            }
            // Inject duplicates of random batches at random positions.
            for _ in 0..(rng.next() % 5) {
                let dup = (rng.next() as usize) % batches.len();
                let at = (rng.next() as usize) % (order.len() + 1);
                order.insert(at, dup);
            }

            let router = fixture_router();
            let mut rx = router_with_consumer(&router, "r");
            deliver_all(&router, "r", &batches, &order);
            assert_eq!(
                accumulate_consumer(&mut rx).unwrap_or_else(|e| panic!("seed {seed}: {e}")),
                expected,
                "seed {seed}"
            );
        }
    }

    #[tokio::test]
    async fn heartbeat_during_gap_is_delivered_immediately_without_releasing_held_data() {
        let router = fixture_router();
        let mut rx = router_with_consumer(&router, "r");

        deliver(&router, "r", 1, text("b"));
        router
            .requests
            .lock()
            .get_mut("r")
            .expect("request exists")
            .route(
                None,
                StreamRelayMessage::Heartbeat {
                    phase: crate::ProviderStreamPhase::Streaming,
                    elapsed_ms: 1,
                    idle_ms: 0,
                    chunk_count: 0,
                },
            );

        assert!(matches!(
            rx.try_recv(),
            Ok(StreamRelayMessage::Heartbeat { .. })
        ));
        assert!(rx.try_recv().is_err(), "held data must not be released");

        deliver(&router, "r", 0, text("a"));
        assert!(matches!(
            rx.try_recv(),
            Ok(StreamRelayMessage::Chunk(StreamChunk::Text(t))) if t == "a"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(StreamRelayMessage::Chunk(StreamChunk::Text(t))) if t == "b"
        ));
    }

    // ---- gap detection ----

    fn routed(seq: u64, message: StreamRelayMessage) -> RoutedStreamRelayMessage {
        RoutedStreamRelayMessage {
            request_id: "r".to_string(),
            message,
            seq: Some(seq),
        }
    }

    async fn recv_within(
        rx: &mut mpsc::UnboundedReceiver<StreamRelayMessage>,
    ) -> Option<StreamRelayMessage> {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("consumer should receive or close within 3s")
    }

    fn gap_failure_reason(message: StreamRelayMessage) -> String {
        match message {
            StreamRelayMessage::TransportFailed { error } => {
                querymt::error::LLMError::from_payload(error).to_string()
            }
            other => panic!("expected TransportFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unfilled_gap_fails_stream_once_after_timeout() {
        let router = ProviderStreamRouterActor::new(Some(10), Some(60))
            .with_gap_limits(Duration::from_millis(50), 1024);
        let requests = Arc::clone(&router.requests);
        let mut rx = router_with_consumer(&router, "r");
        let router_ref = ProviderStreamRouterActor::spawn(router);

        // seq 0 never arrives.
        router_ref.tell(routed(1, text("b"))).await.expect("tell");
        router_ref.tell(routed(2, text("c"))).await.expect("tell");

        let failure = recv_within(&mut rx).await.expect("failure message");
        let reason = gap_failure_reason(failure);
        assert!(reason.contains("missing seq 0"), "{reason}");
        assert!(reason.contains("held 2"), "{reason}");
        assert!(
            recv_within(&mut rx).await.is_none(),
            "no held data and no second message after the failure"
        );
        let phase = requests.lock().get("r").expect("request").phase;
        assert_eq!(phase, RequestPhase::Failed);
    }

    #[tokio::test]
    async fn held_message_cap_fails_immediately() {
        let router = ProviderStreamRouterActor::new(Some(10), Some(60))
            .with_gap_limits(Duration::from_secs(60), 4);
        let mut rx = router_with_consumer(&router, "r");

        for seq in 1..=4u64 {
            deliver(&router, "r", seq, text("x"));
        }
        assert!(rx.try_recv().is_err(), "four held messages are within cap");
        deliver(&router, "r", 5, text("x"));

        let reason = gap_failure_reason(rx.try_recv().expect("failure on fifth held"));
        assert!(reason.contains("missing seq 0"), "{reason}");
        assert!(rx.try_recv().is_err(), "held data must not be released");
        assert_eq!(router.get_status(Some("r"))[0].phase, RequestPhase::Failed);
    }

    #[tokio::test]
    async fn setup_failure_provider_error_with_seq_zero_reaches_consumer() {
        let router = fixture_router();
        let mut rx = router_with_consumer(&router, "r");
        let error = querymt::error::LLMError::ProviderError("setup failed".into()).to_payload();

        deliver(&router, "r", 0, StreamRelayMessage::ProviderError { error });

        assert!(matches!(
            rx.try_recv(),
            Ok(StreamRelayMessage::ProviderError { .. })
        ));
        assert_eq!(router.get_status(Some("r"))[0].phase, RequestPhase::Failed);
    }

    #[tokio::test]
    async fn gap_filled_before_timeout_raises_nothing() {
        let router = ProviderStreamRouterActor::new(Some(10), Some(60))
            .with_gap_limits(Duration::from_millis(100), 1024);
        let requests = Arc::clone(&router.requests);
        let mut rx = router_with_consumer(&router, "r");
        let router_ref = ProviderStreamRouterActor::spawn(router);

        router_ref.tell(routed(1, text("b"))).await.expect("tell");
        router_ref.tell(routed(0, text("a"))).await.expect("tell");
        // Wait past the timeout so the scheduled check has run.
        tokio::time::sleep(Duration::from_millis(250)).await;

        for expected in ["a", "b"] {
            assert!(matches!(
                rx.try_recv(),
                Ok(StreamRelayMessage::Chunk(StreamChunk::Text(t))) if t == expected
            ));
        }
        assert!(rx.try_recv().is_err(), "no failure expected");
        let phase = requests.lock().get("r").expect("request").phase;
        assert_eq!(phase, RequestPhase::Streaming);
    }
}
