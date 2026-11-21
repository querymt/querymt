use super::{RelayQueue, RemoteProviderHostError, validate_item_aware_request};
use crate::{
    ProviderStreamPhase, ProviderStreamRequest, ProviderStreamRouterActor, ProviderStreamStatus,
    RoutedStreamRelayMessage, StreamRelayMessage, should_ack_relay_message,
};
use futures_util::StreamExt;
use kameo::actor::RemoteActorRef;
use parking_lot::Mutex;
use querymt::chat::{ChatCall, ChatStream, StreamChunk, chunk_is_terminal};
use querymt::error::{LLMError, TransportErrorKind};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, oneshot};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "kameo-mesh")]
use crate::{
    MeshHandle,
    mesh_ingress::PeerRequest,
    metered_provider::MeteredAccess,
    peer_access::{PeerGrantKey, SharingProtocol},
    request_timeout::ask_sharing_v1,
};

pub(super) type StreamRequest = ProviderStreamRequest<RemoteActorRef<ProviderStreamRouterActor>>;
pub(super) type LiveStreams = Arc<Mutex<HashMap<StreamKey, Arc<ActiveStream>>>>;
const RELAY_TIMEOUT: Duration = Duration::from_secs(3);
const BATCH_FLUSH_INTERVAL: Duration = Duration::from_millis(25);
const MAX_BATCH_SIZE: usize = 16;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum Owner {
    Local,
    #[cfg(feature = "kameo-mesh")]
    Peer(PeerGrantKey, SharingProtocol),
}

impl Owner {
    #[cfg(feature = "kameo-mesh")]
    pub(super) fn peer(peer: &PeerRequest) -> Result<Self, RemoteProviderHostError> {
        if peer.grant().is_none() {
            return Err(super::quota_error(
                crate::peer_access::quota::QuotaError::Denied,
            ));
        }
        Ok(Self::Peer(peer.key().clone(), peer.protocol()))
    }

    fn guarded(&self) -> bool {
        !matches!(self, Self::Local)
    }

    fn duration(&self, secs: u64) -> Duration {
        Duration::from_secs(if self.guarded() {
            secs.clamp(1, 300)
        } else {
            secs.max(1)
        })
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct StreamKey {
    owner: Owner,
    session_id: String,
    request_id: String,
}

pub(super) struct ActiveStream {
    key: StreamKey,
    provider: String,
    model: String,
    started: Instant,
    progress: Mutex<Progress>,
    cancel: CancellationToken,
    lease_changed: Notify,
    #[cfg(feature = "kameo-mesh")]
    access: Option<MeteredAccess>,
}

struct Progress {
    lease_expires: Instant,
    last_progress: Instant,
    chunks: u64,
    phase: ProviderStreamPhase,
    connected: bool,
    last_error: Option<String>,
}

pub(super) fn matching(
    streams: &LiveStreams,
    owner: &Owner,
    session: &str,
    request: Option<&str>,
) -> Vec<Arc<ActiveStream>> {
    streams
        .lock()
        .iter()
        .filter(|(key, _)| {
            &key.owner == owner
                && key.session_id == session
                && request.is_none_or(|id| key.request_id == id)
        })
        .map(|(_, stream)| stream.clone())
        .collect()
}

impl ActiveStream {
    pub(super) fn status(&self) -> ProviderStreamStatus {
        let progress = self.progress.lock();
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        ProviderStreamStatus {
            session_id: self.key.session_id.clone(),
            request_id: self.key.request_id.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            phase: progress.phase,
            elapsed_ms: millis(self.started.elapsed()),
            idle_ms: millis(progress.last_progress.elapsed()),
            chunk_count: progress.chunks,
            receiver_connected: progress.connected,
            lease_expires_in_ms: millis(
                progress
                    .lease_expires
                    .saturating_duration_since(Instant::now()),
            ),
            last_error: progress.last_error.clone(),
        }
    }

    pub(super) fn cancel(&self, reason: Option<&str>) {
        if !self.key.owner.guarded() {
            let mut progress = self.progress.lock();
            progress.phase = ProviderStreamPhase::Cancelling;
            progress.last_error = Some(reason.unwrap_or("cancel requested").to_owned());
        }
        self.cancel.cancel();
    }

    pub(super) fn renew(&self, seconds: u64) -> bool {
        let mut progress = self.progress.lock();
        let now = Instant::now();
        if self.key.owner.guarded() && (now >= progress.lease_expires || self.cancel.is_cancelled())
        {
            return false;
        }
        progress.lease_expires = now + self.key.owner.duration(seconds);
        self.lease_changed.notify_one();
        true
    }

    pub(super) async fn authorize(&self) -> Result<(), RemoteProviderHostError> {
        #[cfg(feature = "kameo-mesh")]
        if let Some(access) = &self.access {
            access.check().await.map_err(super::policy_error)?;
        }
        Ok(())
    }

    async fn interrupted(&self) -> LLMError {
        let lease = async {
            loop {
                let deadline = self.progress.lock().lease_expires;
                if Instant::now() >= deadline {
                    if !self.key.owner.guarded() {
                        let mut progress = self.progress.lock();
                        // Renew and expiry must agree on which deadline won.
                        if Instant::now() < progress.lease_expires {
                            continue;
                        }
                        progress.phase = ProviderStreamPhase::LeaseExpired;
                        progress.last_error = Some("stream lease expired".into());
                        return transport(
                            TransportErrorKind::ConnectionClosed,
                            "provider stream cancelled",
                        );
                    }
                    return transport(
                        TransportErrorKind::Timeout,
                        "provider stream lease or deadline expired",
                    );
                }
                tokio::select! {
                    _ = time::sleep_until(deadline) => {},
                    _ = self.lease_changed.notified() => {},
                }
            }
        };
        let revoked = async {
            #[cfg(feature = "kameo-mesh")]
            if let Some(access) = &self.access {
                return access.revocation().await;
            }
            std::future::pending().await
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => transport(TransportErrorKind::ConnectionClosed, "provider stream cancelled"),
            error = revoked => error,
            error = lease => error,
        }
    }
}

pub(super) struct Relay {
    streams: LiveStreams,
    active: Arc<ActiveStream>,
    receiver: RemoteActorRef<ProviderStreamRouterActor>,
    grace: Duration,
    heartbeat: Duration,
    #[cfg(feature = "kameo-mesh")]
    mesh: Option<MeshHandle>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl Relay {
    pub(super) fn new(
        streams: LiveStreams,
        req: &StreamRequest,
        owner: Owner,
        cancel: CancellationToken,
        #[cfg(feature = "kameo-mesh")] guarded: Option<(MeshHandle, MeteredAccess)>,
    ) -> Result<Self, RemoteProviderHostError> {
        let now = Instant::now();
        let lease = owner.duration(req.lease_ttl_secs);
        let grace = owner.duration(req.reconnect_grace_secs);
        let heartbeat = owner.duration(req.heartbeat_interval_secs);
        #[cfg(feature = "kameo-mesh")]
        let (mesh, access) = guarded.unzip();
        let key = StreamKey {
            owner,
            session_id: req.session_id.clone(),
            request_id: req.request_id.clone(),
        };
        let active = Arc::new(ActiveStream {
            key: key.clone(),
            provider: req.provider.clone(),
            model: req.model.clone(),
            started: now,
            cancel,
            lease_changed: Notify::new(),
            progress: Mutex::new(Progress {
                lease_expires: now + lease,
                last_progress: now,
                chunks: 0,
                phase: ProviderStreamPhase::OpeningUpstream,
                connected: true,
                last_error: None,
            }),
            #[cfg(feature = "kameo-mesh")]
            access,
        });
        {
            let mut lock = streams.lock();
            match lock.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(active.clone());
                }
                std::collections::hash_map::Entry::Occupied(_) => {
                    return Err(RemoteProviderHostError::provider_chat_message(
                        "chat_stream",
                        "provider stream already active",
                    ));
                }
            }
        }
        Ok(Self {
            streams,
            active,
            receiver: req.stream_router_ref.clone(),
            grace,
            heartbeat,
            #[cfg(feature = "kameo-mesh")]
            mesh,
        })
    }

    fn unregister(&self) {
        let mut streams = self.streams.lock();
        if let std::collections::hash_map::Entry::Occupied(entry) =
            streams.entry(self.active.key.clone())
            && Arc::ptr_eq(entry.get(), &self.active)
        {
            entry.remove();
        }
    }

    fn guarded(&self) -> bool {
        self.active.key.owner.guarded()
    }

    pub(super) async fn open_local(
        &self,
        backend: &dyn crate::RemoteProviderBackend<Error = RemoteProviderHostError>,
        msg: StreamRequest,
    ) -> Result<ChatStream, RemoteProviderHostError> {
        tokio::select! {
            biased;
            error = self.active.interrupted() => Err(host_error(&error)),
            result = async {
                validate_item_aware_request(&msg.messages, msg.item_aware_contract_version)?;
                let provider = crate::build_provider_for_request(backend, &msg.provider, &msg.model, msg.params.as_ref()).await?;
                provider.chat_stream(&call(&msg)).await.map_err(|error| host_error(&error))
            } => result,
        }
    }

    pub(super) fn local_open_failed(&self, error: RemoteProviderHostError) {
        let _ = self
            .receiver
            .tell(&self.frame(
                StreamRelayMessage::ProviderError {
                    error: error.to_payload(),
                },
                Some(0),
            ))
            .send();
    }

    #[tracing::instrument(name = "remote.provider_host.stream.relay", skip_all, fields(
        provider = %self.active.provider,
        model = %self.active.model,
        session_id = %self.active.key.session_id,
        request_id = %self.active.key.request_id,
    ))]
    pub(super) async fn run(
        self,
        open: impl Future<Output = Result<ChatStream, RemoteProviderHostError>>,
        settled: Option<oneshot::Receiver<()>>,
    ) {
        let mut delivery = Delivery::default();
        let outcome = tokio::select! {
            biased;
            error = self.active.interrupted() => Err((host_error(&error), true)),
            result = async {
                let stream = open.await.map_err(|error| (error, false))?;
                self.forward(stream, &mut delivery).await.map_err(|error| (error, false))
            } => result,
        };
        // If an interruption dropped the upstream read, the quota guard still
        // owns the accounting. Wait for it before sending the terminal.
        let outcome = if let Some(settled) = settled {
            match (outcome, settled.await) {
                (Ok(_), Err(error)) => Err((host_error(&crate_quota_error(error)), false)),
                (outcome, _) => outcome,
            }
        } else {
            outcome
        };
        if self.guarded() {
            // Guarded failures must pass a held gap, without replaying data.
            delivery.queue.clear();
        }
        let (terminal, sequenced) = match outcome {
            Ok(Some(message)) => (message, true),
            Ok(None) => return,
            Err((error, interrupted)) => {
                let terminal = if !self.guarded() && interrupted {
                    if !delivery.batch.is_empty() {
                        delivery.queue.push_sequenced(
                            self.batch_message(std::mem::take(&mut delivery.batch)),
                        );
                    }
                    StreamRelayMessage::TransportFailed {
                        error: error.to_payload(),
                    }
                } else {
                    StreamRelayMessage::ProviderError {
                        error: error.to_payload(),
                    }
                };
                (terminal, !self.guarded())
            }
        };
        if self.guarded() {
            self.unregister();
        }
        let delivered = if self.guarded() {
            matches!(
                time::timeout(
                    RELAY_TIMEOUT + self.grace,
                    self.deliver(&mut delivery, terminal, sequenced)
                )
                .await,
                Ok(Ok(()))
            )
        } else if delivery.grace_expired {
            false
        } else {
            self.deliver(&mut delivery, terminal, sequenced)
                .await
                .is_ok()
        };
        if !delivered && let Err(error) = self.active.authorize().await {
            // Denials must pass a held gap without leaking data from the binding.
            let _ = self
                .send(
                    self.frame(
                        StreamRelayMessage::ProviderError {
                            error: error.to_payload(),
                        },
                        None,
                    ),
                    true,
                )
                .await;
        }
    }

    async fn forward(
        &self,
        mut stream: ChatStream,
        delivery: &mut Delivery,
    ) -> Result<Option<StreamRelayMessage>, RemoteProviderHostError> {
        self.active.progress.lock().phase = ProviderStreamPhase::WaitingFirstChunk;
        let mut heartbeat = time::interval_at(Instant::now() + self.heartbeat, self.heartbeat);
        heartbeat.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        let mut flush_at = Instant::now();
        loop {
            tokio::select! {
                _ = heartbeat.tick(), if delivery.batch.is_empty() => {
                    let status = self.active.status();
                    self.deliver(delivery, StreamRelayMessage::Heartbeat { phase: status.phase, elapsed_ms: status.elapsed_ms, idle_ms: status.idle_ms, chunk_count: status.chunk_count }, false).await?;
                }
                _ = time::sleep_until(flush_at), if !delivery.batch.is_empty() => {
                    self.flush(delivery).await?;
                }
                item = stream.next() => match item {
                    Some(Ok(chunk)) => {
                        if delivery.batch.is_empty() { flush_at = Instant::now() + BATCH_FLUSH_INTERVAL; }
                        let terminal = chunk_is_terminal(&chunk);
                        delivery.batch.push(chunk);
                        if terminal {
                            if !self.guarded() { self.active.progress.lock().phase = ProviderStreamPhase::Completed; }
                            return Ok(Some(self.batch_message(std::mem::take(&mut delivery.batch))));
                        }
                        if delivery.batch.len() >= MAX_BATCH_SIZE { self.flush(delivery).await?; }
                    }
                    Some(Err(error)) => {
                        self.flush(delivery).await?;
                        if !self.guarded() {
                            let mut progress = self.active.progress.lock();
                            progress.phase = ProviderStreamPhase::Failed;
                            progress.last_error = Some(error.to_string());
                        }
                        return Err(host_error(&error));
                    }
                    None => {
                        self.flush(delivery).await?;
                        return if self.guarded() {
                            Err(host_error(&transport(TransportErrorKind::ConnectionClosed, "provider ended without a terminal chunk")))
                        } else { Ok(None) };
                    }
                }
            }
        }
    }

    fn batch_message(&self, mut batch: Vec<StreamChunk>) -> StreamRelayMessage {
        if !self.guarded() && batch.len() == 1 {
            StreamRelayMessage::Chunk(batch.pop().expect("batch contains one chunk"))
        } else {
            StreamRelayMessage::ChunkBatch(batch)
        }
    }

    async fn flush(&self, delivery: &mut Delivery) -> Result<(), RemoteProviderHostError> {
        if !delivery.batch.is_empty() {
            let count = delivery.batch.len() as u64;
            let message = self.batch_message(std::mem::take(&mut delivery.batch));
            self.deliver(delivery, message, true).await?;
            let mut progress = self.active.progress.lock();
            progress.phase = ProviderStreamPhase::Streaming;
            progress.last_progress = Instant::now();
            progress.chunks = progress.chunks.saturating_add(count);
        }
        Ok(())
    }

    async fn deliver(
        &self,
        delivery: &mut Delivery,
        message: StreamRelayMessage,
        sequenced: bool,
    ) -> Result<(), RemoteProviderHostError> {
        if sequenced {
            delivery.queue.push_sequenced(message);
        } else {
            delivery.queue.push_unsequenced(message);
        }
        while let Some((seq, message)) = delivery.queue.front_cloned() {
            self.deliver_front(delivery, seq, &message).await?;
            delivery.queue.pop_front();
        }
        Ok(())
    }

    async fn deliver_front(
        &self,
        delivery: &mut Delivery,
        seq: Option<u64>,
        message: &StreamRelayMessage,
    ) -> Result<(), RemoteProviderHostError> {
        let started = Instant::now();
        loop {
            self.active.authorize().await?;
            let connected = self.active.progress.lock().connected;
            let mut ready = connected;
            if !connected {
                let notice = StreamRelayMessage::TransportReconnected {
                    buffered_chunks: if self.guarded() {
                        usize::from(seq.is_some())
                    } else {
                        delivery.queue.replayable_count()
                    },
                };
                if self.send(self.frame(notice, None), true).await.is_ok() {
                    self.active.authorize().await?;
                    delivery.acknowledged();
                    ready = true;
                }
            }
            if ready {
                let ack = self.guarded()
                    || should_ack_relay_message(
                        message,
                        delivery.unacked,
                        delivery.last_ack.elapsed(),
                        8,
                        Duration::from_millis(40),
                    );
                match self.send(self.frame(message.clone(), seq), ack).await {
                    Ok(()) => {
                        if ack
                            || !matches!(
                                message,
                                StreamRelayMessage::Chunk(_) | StreamRelayMessage::ChunkBatch(_)
                            )
                        {
                            delivery.acknowledged();
                        } else {
                            delivery.unacked = delivery.unacked.saturating_add(1);
                        }
                        let mut progress = self.active.progress.lock();
                        progress.connected = true;
                        if progress.phase == ProviderStreamPhase::ReceiverDisconnected {
                            progress.phase = if progress.chunks == 0 {
                                ProviderStreamPhase::WaitingFirstChunk
                            } else {
                                ProviderStreamPhase::Streaming
                            };
                        }
                        delivery.disconnected_at = None;
                        return Ok(());
                    }
                    Err(reason) if connected && !self.guarded() => {
                        let _ = self
                            .receiver
                            .tell(&self.frame(
                                StreamRelayMessage::TransportDisconnected {
                                    reason: reason.clone(),
                                },
                                None,
                            ))
                            .send();
                        let mut progress = self.active.progress.lock();
                        progress.phase = ProviderStreamPhase::ReceiverDisconnected;
                        progress.last_error = Some(reason);
                    }
                    Err(_) => {}
                }
            }
            self.active.progress.lock().connected = false;
            let disconnected = *delivery.disconnected_at.get_or_insert_with(Instant::now);
            let grace_start = if self.guarded() {
                started
            } else {
                disconnected
            };
            if grace_start.elapsed() >= self.grace {
                delivery.grace_expired = true;
                if !self.guarded() {
                    self.active.progress.lock().phase = ProviderStreamPhase::GraceExpired;
                }
                return Err(host_error(&transport(
                    TransportErrorKind::Timeout,
                    "provider stream reconnect grace expired",
                )));
            }
            time::sleep(Duration::from_millis(250)).await;
        }
    }

    fn frame(&self, message: StreamRelayMessage, seq: Option<u64>) -> RoutedStreamRelayMessage {
        RoutedStreamRelayMessage {
            request_id: self.active.key.request_id.clone(),
            message,
            seq,
        }
    }

    async fn send(&self, frame: RoutedStreamRelayMessage, ack: bool) -> Result<(), String> {
        #[cfg(feature = "kameo-mesh")]
        if let Owner::Peer(_, SharingProtocol::V1) = &self.active.key.owner {
            let mesh = self.mesh.as_ref().expect("V1 relay has a mesh handle");
            return ask_sharing_v1::<_, _, (), String>(mesh, &self.receiver, &frame, RELAY_TIMEOUT)
                .await
                .map_err(|error| error.to_string());
        }
        let tell = self.receiver.tell(&frame);
        if self.guarded() {
            time::timeout(RELAY_TIMEOUT, tell.send_ack())
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())
        } else if ack {
            tell.send_ack().await.map_err(|error| error.to_string())
        } else {
            tell.send().map_err(|error| error.to_string())
        }
    }
}

struct Delivery {
    queue: RelayQueue,
    batch: Vec<StreamChunk>,
    disconnected_at: Option<Instant>,
    grace_expired: bool,
    unacked: u32,
    last_ack: Instant,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            queue: RelayQueue::default(),
            batch: Vec::with_capacity(MAX_BATCH_SIZE),
            disconnected_at: None,
            grace_expired: false,
            unacked: 0,
            last_ack: Instant::now(),
        }
    }
}

impl Delivery {
    fn acknowledged(&mut self) {
        self.unacked = 0;
        self.last_ack = Instant::now();
    }
}

pub(super) fn call(msg: &StreamRequest) -> ChatCall<'_> {
    ChatCall::new(&msg.messages)
        .maybe_tools(msg.tools.as_deref())
        .options(msg.options.unwrap_or_default())
}

fn host_error(error: &LLMError) -> RemoteProviderHostError {
    RemoteProviderHostError::provider_chat("chat_stream", error)
}

fn crate_quota_error(error: oneshot::error::RecvError) -> LLMError {
    crate::peer_access::quota::QuotaError::Store(crate::peer_access::GrantStoreError::Backend(
        Box::new(error),
    ))
    .into_provider_error()
}

fn transport(kind: TransportErrorKind, message: &str) -> LLMError {
    LLMError::Transport {
        kind,
        message: message.to_owned(),
    }
}
