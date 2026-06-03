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
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::mpsc;

const DEFAULT_REPLAY_BUFFER_SIZE: usize = 1000;
const DEFAULT_REQUEST_TTL_SECS: u64 = 300;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RouterError {
    #[error("router invariant violated: {0}")]
    Invariant(String),
}

#[derive(Debug)]
pub struct AttachStreamConsumer {
    pub request_id: String,
    pub consumer_tx: mpsc::Sender<StreamRelayMessage>,
}

#[derive(Debug, Clone)]
pub struct RegisterRequest {
    pub request_id: String,
}

#[derive(Debug, Clone)]
pub struct DetachStreamConsumer {
    pub request_id: String,
}

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

    fn deliver_or_buffer(&mut self, message: StreamRelayMessage) {
        self.last_message_at = Instant::now();

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
            match tx.try_send(message) {
                Ok(()) => {
                    if is_terminal {
                        if let Some(phase) = terminal_phase {
                            self.phase = phase;
                        }
                        self.consumer_tx = None;
                    }
                    return;
                }
                Err(mpsc::error::TrySendError::Full(msg)) => {
                    self.phase = RequestPhase::ConsumerDisconnected;
                    msg
                }
                Err(mpsc::error::TrySendError::Closed(msg)) => {
                    self.consumer_tx = None;
                    self.phase = RequestPhase::ConsumerDisconnected;
                    msg
                }
            }
        } else {
            message
        };

        if self.replay_buffer.len() >= self.replay_buffer_size {
            self.replay_buffer.pop_front();
        }
        self.replay_buffer.push_back(message);

        if relay_message_is_terminal(self.replay_buffer.back().expect("buffer contains pushed message"))
            && let Some(phase) = terminal_phase
        {
            self.phase = phase;
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
}

impl ProviderStreamRouterActor {
    pub fn new(replay_buffer_size: Option<usize>, request_ttl_secs: Option<u64>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(HashMap::new())),
            replay_buffer_size: replay_buffer_size.unwrap_or(DEFAULT_REPLAY_BUFFER_SIZE),
            request_ttl: Duration::from_secs(request_ttl_secs.unwrap_or(DEFAULT_REQUEST_TTL_SECS)),
        }
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
            RoutedRequest::new(request_id, self.replay_buffer_size),
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

        request.deliver_or_buffer(msg.message);
        tracing::trace!(
            target: "querymt_remote::provider::router",
            request_id = %msg.request_id,
            phase = %request.phase,
            has_consumer = request.consumer_tx.is_some(),
            buffered = request.replay_buffer.len(),
            "routed message delivered or buffered"
        );
        drop(requests);
        self.cleanup_expired_requests();
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
            match consumer_tx.try_send(message) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(msg)) => {
                    request.replay_buffer.push_front(msg);
                    request.phase = RequestPhase::ConsumerDisconnected;
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(msg)) => {
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
            Box::pin(_internal::unlink::<ProviderStreamRouterActor>(actor_id, sibling_id))
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
                    Box::pin(_internal::tell::<$actor, $msg_ty>(actor_id, msg, mailbox_timeout))
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
        router.register_request(request_id.clone()).expect("register request");
        let status = router.get_status(Some(&request_id));
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].request_id, request_id);
        assert_eq!(status[0].phase, RequestPhase::AwaitingStream);
    }

    #[tokio::test]
    async fn attach_requires_preregistration() {
        let router_ref = ProviderStreamRouterActor::spawn(create_test_router());
        let (tx, _rx) = mpsc::channel(8);
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
        router.register_request(request_id.clone()).expect("register request");
        router
            .requests
            .lock()
            .get_mut(&request_id)
            .expect("request exists")
            .deliver_or_buffer(StreamRelayMessage::Chunk(StreamChunk::Done {
                finish_reason: querymt::chat::FinishReason::Stop,
            }));

        let router_ref = ProviderStreamRouterActor::spawn(router);
        let (tx, mut rx) = mpsc::channel(8);
        router_ref
            .ask(AttachStreamConsumer {
                request_id,
                consumer_tx: tx,
            })
            .await
            .expect("attach should succeed");

        assert!(matches!(rx.recv().await, Some(StreamRelayMessage::Chunk(StreamChunk::Done { .. }))));
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn buffer_overflow_evicts_oldest_message() {
        let router = ProviderStreamRouterActor::new(Some(3), Some(60));
        let request_id = "test-request-3".to_string();
        router.register_request(request_id.clone()).expect("register request");

        for i in 0..4 {
            router
                .requests
                .lock()
                .get_mut(&request_id)
                .expect("request exists")
                .deliver_or_buffer(StreamRelayMessage::Chunk(StreamChunk::Text(format!("msg-{i}"))));
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
        let (tx, _rx) = mpsc::channel(8);
        router_ref
            .ask(AttachStreamConsumer {
                request_id: request_id.clone(),
                consumer_tx: tx,
            })
            .await
            .expect("attach should succeed");

        router_ref.tell(DetachStreamConsumer { request_id: request_id.clone() }).await.expect("detach");

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
        router.register_request(request_id.clone()).expect("register request");
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
        router.register_request(request_id.clone()).expect("register request");
        let (tx, mut rx) = mpsc::channel(8);
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

        assert!(matches!(rx.recv().await, Some(StreamRelayMessage::Chunk(StreamChunk::Done { .. }))));
        assert!(rx.recv().await.is_none());
        let status = router.get_status(Some(&request_id));
        assert_eq!(status[0].phase, RequestPhase::Completed);
        assert!(!status[0].has_consumer);
    }

    #[tokio::test]
    async fn get_status_without_request_id_returns_all_requests() {
        let router = create_test_router();
        router.register_request("req-a".to_string()).expect("register req-a");
        router.register_request("req-b".to_string()).expect("register req-b");

        let mut ids = router
            .get_status(None)
            .into_iter()
            .map(|status| status.request_id)
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(ids, vec!["req-a".to_string(), "req-b".to_string()]);
    }
}
