//! Peer-owned provider streams. The legacy actor remains separate: its local
//! controls and unauthenticated wire handlers cannot address these streams.

use super::{
    GuardedProvider, enforce_settlement, failure, quota_error, validate_item_aware_request,
};
use crate::mesh_ingress::{InboundAccess, InboundRouter, PeerRequest};
use crate::peer_access::quota::{AttemptUsage, QuotaError, ReservedAttempt};
use crate::peer_access::{PeerGrantKey, SharingProtocol};
use crate::request_timeout::ask_sharing_v1;
use crate::{
    CancelProviderStreamRequest, GetProviderStreamStatus, MeshHandle, ProviderHostActor,
    ProviderStreamPhase, ProviderStreamRequest, ProviderStreamRouterActor, ProviderStreamStatus,
    RemoteProviderHostError, RenewProviderStreamLease, RoutedStreamRelayMessage,
    StreamRelayMessage,
};
use futures_util::StreamExt;
use kameo::actor::{ActorId, RemoteActorRef};
use parking_lot::Mutex;
use querymt::chat::{ChatCall, ChatStream, StreamChunk, StructuredStreamEvent, chunk_is_terminal};
use querymt::error::{LLMError, ProviderErrorKind, TransportErrorKind};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const AUTH_INTERVAL: Duration = Duration::from_secs(1);
const RELAY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_LEASE_SECS: u64 = 300;
const MAX_STREAM_DURATION: Duration = Duration::from_secs(600);

type StreamRequest = ProviderStreamRequest<RemoteActorRef<ProviderStreamRouterActor>>;

#[derive(Clone, PartialEq, Eq, Hash)]
struct StreamKey {
    peer: PeerGrantKey,
    session_id: String,
    request_id: String,
}

struct ActiveStream {
    provider: String,
    model: String,
    revision: Option<Uuid>,
    protocol: SharingProtocol,
    started: Instant,
    state: Mutex<Progress>,
    cancel: CancellationToken,
}

struct Progress {
    lease_expires: Instant,
    last_progress: Instant,
    chunks: u64,
    phase: ProviderStreamPhase,
    connected: bool,
}

impl ActiveStream {
    fn owned_by(&self, peer: &PeerRequest) -> bool {
        self.revision == peer.revision() && self.protocol == peer.protocol()
    }

    fn status(&self, session_id: &str, request_id: &str) -> ProviderStreamStatus {
        let state = self.state.lock();
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        ProviderStreamStatus {
            session_id: session_id.to_owned(),
            request_id: request_id.to_owned(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            phase: state.phase,
            elapsed_ms: millis(self.started.elapsed()),
            idle_ms: millis(state.last_progress.elapsed()),
            chunk_count: state.chunks,
            receiver_connected: state.connected,
            lease_expires_in_ms: millis(
                state
                    .lease_expires
                    .saturating_duration_since(Instant::now()),
            ),
            last_error: None,
        }
    }
}

struct StreamService {
    provider: Arc<GuardedProvider>,
    mesh: MeshHandle,
    active: Mutex<HashMap<StreamKey, Arc<ActiveStream>>>,
}

pub(in crate::provider_share) fn install(
    provider: Arc<GuardedProvider>,
    mesh: &MeshHandle,
    router: &InboundRouter,
    host: ActorId,
) {
    let service = Arc::new(StreamService {
        provider,
        mesh: {
            // The router owns these handlers. Do not retain it through egress.
            let mut egress = mesh.clone();
            egress.policy = None;
            egress
        },
        active: Mutex::new(HashMap::new()),
    });
    let start = |service: Arc<StreamService>| {
        move |peer, msg| {
            let service = service.clone();
            async move { service.start(peer, msg).await }
        }
    };
    router.register_legacy_tell::<ProviderHostActor, StreamRequest, _, _, _>(
        host,
        InboundAccess::LegacyFull,
        start(service.clone()),
    );
    router.register_v1_ask::<ProviderHostActor, StreamRequest, _, _, _, _>(
        host,
        InboundAccess::Providers,
        start(service.clone()),
    );
    macro_rules! control {
        ($message:ty, $method:ident) => {{
            let service = service.clone();
            let handler = move |peer, msg| {
                let service = service.clone();
                async move { service.$method(&peer, msg).await }
            };
            router.register_ask::<ProviderHostActor, $message, _, _, _, _>(
                host,
                InboundAccess::LegacyFull,
                handler.clone(),
            );
            router.register_v1_ask::<ProviderHostActor, $message, _, _, _, _>(
                host,
                InboundAccess::Providers,
                handler,
            );
        }};
    }
    control!(GetProviderStreamStatus, status);
    control!(RenewProviderStreamLease, renew);
    control!(CancelProviderStreamRequest, cancel);
}

impl StreamService {
    async fn start(
        self: &Arc<Self>,
        peer: PeerRequest,
        msg: StreamRequest,
    ) -> Result<(), RemoteProviderHostError> {
        if msg.stream_router_ref.id().peer_id() != Some(peer.key().peer.as_peer_id()) {
            return Err(quota_error(QuotaError::Denied));
        }
        let id = Uuid::parse_str(&msg.request_id).map_err(|_| {
            failure(
                ProviderErrorKind::InvalidRequest,
                "mesh_stream_id",
                "invalid stream request ID",
            )
        })?;
        validate_item_aware_request(&msg.messages, msg.item_aware_contract_version)?;
        let request = self
            .provider
            .build_request(&msg.provider, &msg.model, msg.params)?;
        let call = ChatCall::new(&msg.messages)
            .maybe_tools(msg.tools.as_deref())
            .options(msg.options.unwrap_or_default());
        let (provider, reservation) = self
            .provider
            .prepare_attempt(&peer, request, &call, id)
            .await?;
        let started = Instant::now();
        let active = Arc::new(ActiveStream {
            provider: msg.provider,
            model: msg.model,
            revision: peer.revision(),
            protocol: peer.protocol(),
            started,
            state: Mutex::new(Progress {
                lease_expires: started + lease(msg.lease_ttl_secs),
                last_progress: started,
                chunks: 0,
                phase: ProviderStreamPhase::OpeningUpstream,
                connected: true,
            }),
            cancel: CancellationToken::new(),
        });
        let key = StreamKey {
            peer: peer.key().clone(),
            session_id: msg.session_id,
            request_id: msg.request_id,
        };
        self.active.lock().insert(key.clone(), active.clone());
        let relay = StreamRelay {
            service: self.clone(),
            peer,
            key,
            active,
            receiver: msg.stream_router_ref,
            reconnect_grace: Duration::from_secs(msg.reconnect_grace_secs.clamp(1, MAX_LEASE_SECS)),
            heartbeat: Duration::from_secs(msg.heartbeat_interval_secs.clamp(1, MAX_LEASE_SECS)),
        };
        tokio::spawn(async move {
            let call = ChatCall::new(&msg.messages)
                .maybe_tools(msg.tools.as_deref())
                .options(msg.options.unwrap_or_default());
            relay.run(provider, call, reservation).await;
        });
        Ok(())
    }

    fn matching(
        &self,
        peer: &PeerRequest,
        session: &str,
        request: Option<&str>,
    ) -> Vec<(String, Arc<ActiveStream>)> {
        self.active
            .lock()
            .iter()
            .filter(|(key, stream)| {
                &key.peer == peer.key()
                    && key.session_id == session
                    && request.is_none_or(|id| id == key.request_id)
                    && stream.owned_by(peer)
            })
            .map(|(key, stream)| (key.request_id.clone(), stream.clone()))
            .collect()
    }

    async fn status(
        &self,
        peer: &PeerRequest,
        msg: GetProviderStreamStatus,
    ) -> Result<Option<ProviderStreamStatus>, RemoteProviderHostError> {
        if let Some((request_id, stream)) = self
            .matching(peer, &msg.session_id, msg.request_id.as_deref())
            .into_iter()
            .next()
        {
            self.provider
                .binding(peer, &stream.provider, &stream.model)
                .await?;
            return Ok(Some(stream.status(&msg.session_id, &request_id)));
        }
        Ok(None)
    }

    async fn cancel(
        &self,
        peer: &PeerRequest,
        msg: CancelProviderStreamRequest,
    ) -> Result<usize, RemoteProviderHostError> {
        let mut count = 0;
        for (_, stream) in self.matching(peer, &msg.session_id, msg.request_id.as_deref()) {
            self.provider
                .binding(peer, &stream.provider, &stream.model)
                .await?;
            stream.cancel.cancel();
            count += 1;
        }
        Ok(count)
    }

    async fn renew(
        &self,
        peer: &PeerRequest,
        msg: RenewProviderStreamLease,
    ) -> Result<bool, RemoteProviderHostError> {
        let Some((_, stream)) = self
            .matching(peer, &msg.session_id, Some(&msg.request_id))
            .pop()
        else {
            return Ok(false);
        };
        self.provider
            .binding(peer, &stream.provider, &stream.model)
            .await?;
        let mut state = stream.state.lock();
        let now = Instant::now();
        if now >= state.lease_expires || stream.cancel.is_cancelled() {
            return Ok(false);
        }
        state.lease_expires = now + lease(msg.lease_ttl_secs);
        Ok(true)
    }
}

fn lease(seconds: u64) -> Duration {
    Duration::from_secs(seconds.clamp(1, MAX_LEASE_SECS))
}

struct StreamRelay {
    service: Arc<StreamService>,
    peer: PeerRequest,
    key: StreamKey,
    active: Arc<ActiveStream>,
    receiver: RemoteActorRef<ProviderStreamRouterActor>,
    reconnect_grace: Duration,
    heartbeat: Duration,
}

impl StreamRelay {
    async fn authorized(&self) -> Result<(), RemoteProviderHostError> {
        self.service
            .provider
            .binding(&self.peer, &self.active.provider, &self.active.model)
            .await?;
        Ok(())
    }

    async fn watch(&self) -> RemoteProviderHostError {
        loop {
            let now = Instant::now();
            if now >= self.active.state.lock().lease_expires
                || now.duration_since(self.active.started) >= MAX_STREAM_DURATION
            {
                return RemoteProviderHostError::provider_chat(
                    "chat_stream",
                    &transport(
                        TransportErrorKind::Timeout,
                        "provider stream lease or deadline expired",
                    ),
                );
            }
            if let Err(error) = self.authorized().await {
                return error;
            }
            time::sleep(AUTH_INTERVAL).await;
        }
    }

    async fn run(
        self,
        provider: Arc<dyn querymt::LLMProvider>,
        call: ChatCall<'_>,
        reservation: ReservedAttempt,
    ) {
        let mut meter = StreamMeter::default();
        let mut seq = 0;
        let ready = if self.active.cancel.is_cancelled() {
            Err(cancelled())
        } else {
            self.authorized().await
        };
        let (outcome, settled) = match ready {
            Err(error) => (Err(error), reservation.release().await.map_err(quota_error)),
            Ok(()) => match reservation.begin().await {
                Err(error) => (Err(quota_error(error)), Ok(())),
                Ok(attempt) => {
                    let outcome = tokio::select! {
                        biased;
                        _ = self.active.cancel.cancelled() => Err(cancelled()),
                        error = self.watch() => Err(error),
                        result = async {
                            self.authorized().await?;
                            let stream = provider.chat_stream(&call).await
                                .map_err(|error| RemoteProviderHostError::provider_chat("chat_stream", &error))?;
                            self.forward(stream, &mut meter, &mut seq).await
                        } => result,
                    };
                    let settled = attempt
                        .settle(meter.complete_usage())
                        .await
                        .map_err(quota_error)
                        .and_then(enforce_settlement);
                    (outcome, settled)
                }
            },
        };
        self.service.active.lock().remove(&self.key);
        let (terminal, seq) = match (outcome, settled) {
            (_, Err(error)) | (Err(error), _) => (
                StreamRelayMessage::ProviderError {
                    error: error.to_payload(),
                },
                None,
            ),
            (Ok(message), Ok(())) => (message, Some(seq)),
        };
        // Settlement is complete before terminal delivery. Retry delivery only,
        // never inference, and recheck the grant before every retransmission.
        if !matches!(
            time::timeout(
                RELAY_TIMEOUT + self.reconnect_grace,
                self.deliver(terminal, seq),
            )
            .await,
            Ok(Ok(()))
        ) && let Err(error) = self.authorized().await
        {
            // Only an unsequenced denial can pass a held gap. No provider data
            // or error details are sent after access changes.
            let _ = self
                .send(
                    StreamRelayMessage::ProviderError {
                        error: error.to_payload(),
                    },
                    None,
                )
                .await;
        }
    }

    async fn forward(
        &self,
        mut stream: ChatStream,
        meter: &mut StreamMeter,
        seq: &mut u64,
    ) -> Result<StreamRelayMessage, RemoteProviderHostError> {
        self.active.state.lock().phase = ProviderStreamPhase::WaitingFirstChunk;
        let mut heartbeat = time::interval_at(Instant::now() + self.heartbeat, self.heartbeat);
        heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        let mut batch = Vec::with_capacity(16);
        loop {
            let item = tokio::select! {
                item = stream.next() => item,
                _ = heartbeat.tick(), if batch.is_empty() => {
                    let status = self.active.status(&self.key.session_id, &self.key.request_id);
                    self.deliver(StreamRelayMessage::Heartbeat { phase: status.phase, elapsed_ms: status.elapsed_ms, idle_ms: status.idle_ms, chunk_count: status.chunk_count }, None).await?;
                    continue;
                }
                _ = time::sleep(Duration::from_millis(25)), if !batch.is_empty() => {
                    self.deliver_batch(&mut batch, seq).await?;
                    continue;
                }
            };
            match item {
                Some(Ok(chunk)) => {
                    meter.observe(&chunk);
                    let terminal = chunk_is_terminal(&chunk);
                    batch.push(chunk);
                    if terminal {
                        return Ok(StreamRelayMessage::ChunkBatch(batch));
                    }
                    if batch.len() == 16 {
                        self.deliver_batch(&mut batch, seq).await?;
                    }
                }
                Some(Err(error)) => {
                    self.deliver_batch(&mut batch, seq).await?;
                    return Err(RemoteProviderHostError::provider_chat(
                        "chat_stream",
                        &error,
                    ));
                }
                None => {
                    self.deliver_batch(&mut batch, seq).await?;
                    return Err(RemoteProviderHostError::provider_chat(
                        "chat_stream",
                        &transport(
                            TransportErrorKind::ConnectionClosed,
                            "provider ended without a terminal chunk",
                        ),
                    ));
                }
            }
        }
    }

    async fn deliver_batch(
        &self,
        batch: &mut Vec<StreamChunk>,
        seq: &mut u64,
    ) -> Result<(), RemoteProviderHostError> {
        if batch.is_empty() {
            return Ok(());
        }
        let count = batch.len() as u64;
        self.deliver(
            StreamRelayMessage::ChunkBatch(std::mem::take(batch)),
            Some(*seq),
        )
        .await?;
        *seq += 1;
        let mut state = self.active.state.lock();
        state.phase = ProviderStreamPhase::Streaming;
        state.last_progress = Instant::now();
        state.chunks = state.chunks.saturating_add(count);
        Ok(())
    }

    async fn deliver(
        &self,
        message: StreamRelayMessage,
        seq: Option<u64>,
    ) -> Result<(), RemoteProviderHostError> {
        let deadline = Instant::now() + self.reconnect_grace;
        loop {
            self.authorized().await?;
            let connected = self.active.state.lock().connected;
            let ready = if connected {
                true
            } else if self
                .send(
                    StreamRelayMessage::TransportReconnected {
                        buffered_chunks: usize::from(seq.is_some()),
                    },
                    None,
                )
                .await
                .is_ok()
            {
                self.authorized().await?;
                true
            } else {
                false
            };
            if ready && self.send(message.clone(), seq).await.is_ok() {
                self.active.state.lock().connected = true;
                return Ok(());
            }
            self.active.state.lock().connected = false;
            if Instant::now() >= deadline {
                return Err(RemoteProviderHostError::provider_chat(
                    "chat_stream",
                    &transport(
                        TransportErrorKind::Timeout,
                        "provider stream reconnect grace expired",
                    ),
                ));
            }
            time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn send(&self, message: StreamRelayMessage, seq: Option<u64>) -> Result<(), ()> {
        let relay = RoutedStreamRelayMessage {
            request_id: self.key.request_id.clone(),
            message,
            seq,
        };
        match self.peer.protocol() {
            SharingProtocol::Legacy => {
                time::timeout(RELAY_TIMEOUT, self.receiver.tell(&relay).send_ack())
                    .await
                    .map_err(|_| ())?
                    .map_err(|_| ())
            }
            SharingProtocol::V1 => ask_sharing_v1::<_, _, (), String>(
                &self.service.mesh,
                &self.receiver,
                &relay,
                RELAY_TIMEOUT,
            )
            .await
            .map_err(|_| ()),
        }
    }
}

fn cancelled() -> RemoteProviderHostError {
    RemoteProviderHostError::provider_chat(
        "chat_stream",
        &transport(
            TransportErrorKind::ConnectionClosed,
            "provider stream cancelled",
        ),
    )
}

fn transport(kind: TransportErrorKind, message: &str) -> LLMError {
    LLMError::Transport {
        kind,
        message: message.to_owned(),
    }
}

#[derive(Default)]
struct StreamMeter {
    usage: Option<querymt::Usage>,
    terminal: bool,
}

impl StreamMeter {
    fn observe(&mut self, chunk: &StreamChunk) {
        self.terminal |= chunk_is_terminal(chunk);
        let usage = match chunk {
            StreamChunk::Usage(usage) => Some(usage),
            StreamChunk::Structured(
                StructuredStreamEvent::ResponseMetadata { usage, .. }
                | StructuredStreamEvent::ResponseTerminal { usage, .. },
            ) => usage.as_ref(),
            _ => None,
        };
        if let Some(usage) = usage {
            self.usage = Some(
                self.usage
                    .take()
                    .map_or_else(|| usage.clone(), |current| current.merge_max(usage.clone())),
            );
        }
    }

    fn complete_usage(&self) -> AttemptUsage {
        match (self.terminal, &self.usage) {
            (true, Some(usage)) => AttemptUsage::from_usage(usage),
            _ => AttemptUsage::Unknown,
        }
    }
}
