use crate::provider_host_error::RemoteProviderHostError;
use crate::provider_protocol::{
    CancelProviderStreamRequest, GetProviderStreamStatus, ProviderChatRequest,
    ProviderChatResponse, ProviderStreamPhase, ProviderStreamRequest, ProviderStreamStatus,
    RenewProviderStreamLease, StreamRelayMessage, keep_stream_message_buffered,
    relay_message_is_terminal, should_ack_relay_message,
};
use crate::stream_router_protocol::RoutedStreamRelayMessage;
use crate::{RemoteProviderBackend, build_provider_for_request};
use kameo::Actor;
use kameo::message::{Context, Message};
use kameo::remote::_internal;
use parking_lot::Mutex;
use querymt::chat::StreamChunk;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

fn update_active_stream(
    active_streams: &Arc<Mutex<HashMap<(String, String), ActiveProviderStream>>>,
    session_id: &str,
    request_id: &str,
    f: impl FnOnce(&mut ActiveProviderStream),
) {
    let key = (session_id.to_string(), request_id.to_string());
    if let Some(stream) = active_streams.lock().get_mut(&key) {
        f(stream);
    }
}

fn remove_active_stream(
    active_streams: &Arc<Mutex<HashMap<(String, String), ActiveProviderStream>>>,
    session_id: &str,
    request_id: &str,
) {
    let key = (session_id.to_string(), request_id.to_string());
    active_streams.lock().remove(&key);
}

#[derive(Debug)]
struct ActiveProviderStream {
    session_id: String,
    request_id: String,
    provider: String,
    model: String,
    phase: ProviderStreamPhase,
    started_at: Instant,
    last_progress_at: Instant,
    last_heartbeat_at: Instant,
    chunk_count: u64,
    receiver_connected: bool,
    lease_expires_at: Instant,
    last_error: Option<String>,
    cancel_token: CancellationToken,
}

impl ActiveProviderStream {
    fn status(&self) -> ProviderStreamStatus {
        let now = Instant::now();
        ProviderStreamStatus {
            session_id: self.session_id.clone(),
            request_id: self.request_id.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            phase: self.phase,
            elapsed_ms: now.duration_since(self.started_at).as_millis() as u64,
            idle_ms: now.duration_since(self.last_progress_at).as_millis() as u64,
            chunk_count: self.chunk_count,
            receiver_connected: self.receiver_connected,
            lease_expires_in_ms: self
                .lease_expires_at
                .checked_duration_since(now)
                .unwrap_or_default()
                .as_millis() as u64,
            last_error: self.last_error.clone(),
        }
    }
}

#[derive(Actor)]
pub struct ProviderHostActor {
    backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>,
    active_streams: Arc<Mutex<HashMap<(String, String), ActiveProviderStream>>>,
}

impl ProviderHostActor {
    pub fn new(backend: Arc<dyn RemoteProviderBackend<Error = RemoteProviderHostError>>) -> Self {
        Self {
            backend,
            active_streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn provider_stream_status(
        &self,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Option<ProviderStreamStatus> {
        let streams = self.active_streams.lock();
        if let Some(request_id) = request_id {
            let key = (session_id.to_string(), request_id.to_string());
            return streams.get(&key).map(ActiveProviderStream::status);
        }

        streams
            .iter()
            .find(|((s, _), _)| s == session_id)
            .map(|(_, stream)| stream.status())
    }

    fn cancel_streams(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        reason: Option<&str>,
    ) -> usize {
        let reason = reason.unwrap_or("cancel requested").to_string();
        let mut streams = self.active_streams.lock();
        let mut cancelled = 0usize;
        let to_cancel: Vec<(String, String)> = streams
            .keys()
            .filter(|(s, r)| s == session_id && request_id.is_none_or(|rid| r == rid))
            .cloned()
            .collect();
        for key in to_cancel {
            if let Some(stream) = streams.get_mut(&key) {
                stream.phase = ProviderStreamPhase::Cancelling;
                stream.last_error = Some(reason.clone());
                stream.cancel_token.cancel();
                cancelled += 1;
            }
        }
        cancelled
    }

    fn renew_stream_lease(&self, session_id: &str, request_id: &str, lease_ttl_secs: u64) -> bool {
        let mut streams = self.active_streams.lock();
        let key = (session_id.to_string(), request_id.to_string());
        let Some(stream) = streams.get_mut(&key) else {
            return false;
        };
        stream.lease_expires_at = Instant::now() + Duration::from_secs(lease_ttl_secs.max(1));
        true
    }
}

impl Message<ProviderChatRequest> for ProviderHostActor {
    type Reply = kameo::reply::DelegatedReply<Result<ProviderChatResponse, RemoteProviderHostError>>;

    #[tracing::instrument(
        name = "remote.provider_host.chat",
        skip(self, ctx),
        fields(
            provider = %msg.provider,
            model = %msg.model,
            message_count = msg.messages.len(),
            has_tools = msg.tools.is_some(),
            has_params = msg.params.is_some(),
            tool_calls_returned = tracing::field::Empty,
            finish_reason = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: ProviderChatRequest,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let backend = Arc::clone(&self.backend);

        ctx.spawn(async move {
            let provider = build_provider_for_request(
                backend.as_ref(),
                &msg.provider,
                &msg.model,
                msg.params.as_ref(),
            )
            .await
            .map_err(|e| RemoteProviderHostError::Internal(e.to_string()))?;

            let tools_slice = msg.tools.as_deref();
            let response = provider
                .chat_with_tools(&msg.messages, tools_slice)
                .await
                .map_err(|e| RemoteProviderHostError::ProviderChat {
                    operation: "chat_with_tools".to_string(),
                    reason: serde_json::to_string(&e.to_payload()).unwrap_or_else(|_| e.to_string()),
                })?;

            let tool_calls = response.tool_calls().unwrap_or_default();
            let finish_reason = response.finish_reason().map(|r| format!("{:?}", r));
            tracing::Span::current()
                .record("tool_calls_returned", tool_calls.len())
                .record("finish_reason", finish_reason.as_deref().unwrap_or("none"));

            Ok(ProviderChatResponse {
                text: response.text(),
                thinking: response.thinking(),
                tool_calls,
                usage: response.usage(),
                finish_reason,
            })
        })
    }
}

impl Message<ProviderStreamRequest<kameo::actor::RemoteActorRef<crate::ProviderStreamRouterActor>>>
    for ProviderHostActor
{
    type Reply = kameo::reply::DelegatedReply<Result<(), RemoteProviderHostError>>;

    #[tracing::instrument(
        name = "remote.provider_host.stream",
        skip(self, ctx),
        fields(
            provider = %msg.provider,
            model = %msg.model,
            session_id = %msg.session_id,
            request_id = %msg.request_id,
            message_count = msg.messages.len(),
            has_tools = msg.tools.is_some(),
            has_params = msg.params.is_some(),
            receiver_found = tracing::field::Empty,
        )
    )]
    async fn handle(
        &mut self,
        msg: ProviderStreamRequest<kameo::actor::RemoteActorRef<crate::ProviderStreamRouterActor>>,
        ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        let backend = Arc::clone(&self.backend);
        let active_streams = Arc::clone(&self.active_streams);

        ctx.spawn(async move {
            use futures_util::StreamExt;

            const MAX_BATCH_SIZE: usize = 16;
            const BATCH_FLUSH_INTERVAL: Duration = Duration::from_millis(25);
            const ACK_WINDOW_BATCHES: u32 = 8;
            const ACK_WINDOW_INTERVAL: Duration = Duration::from_millis(40);

            let provider_name = msg.provider.clone();
            let model = msg.model.clone();
            let session_id = msg.session_id.clone();
            let request_id = msg.request_id.clone();
            let reconnect_grace = Duration::from_secs(msg.reconnect_grace_secs.max(1));
            let heartbeat_interval = Duration::from_secs(msg.heartbeat_interval_secs.max(1));
            let lease_ttl_secs = msg.lease_ttl_secs.max(1);
            let lease_ttl = Duration::from_secs(lease_ttl_secs);
            let cancel_token = CancellationToken::new();
            let started_at = Instant::now();

            {
                let mut streams = active_streams.lock();
                streams.insert(
                    (session_id.clone(), request_id.clone()),
                    ActiveProviderStream {
                        session_id: session_id.clone(),
                        request_id: request_id.clone(),
                        provider: provider_name.clone(),
                        model: model.clone(),
                        phase: ProviderStreamPhase::OpeningUpstream,
                        started_at,
                        last_progress_at: started_at,
                        last_heartbeat_at: started_at,
                        chunk_count: 0,
                        receiver_connected: true,
                        lease_expires_at: started_at + lease_ttl,
                        last_error: None,
                        cancel_token: cancel_token.clone(),
                    },
                );
            }

            let cleanup_request_id = request_id.clone();
            let cleanup_session_id = session_id.clone();
            let cleanup_streams = Arc::clone(&active_streams);
            let stream_router_ref = msg.stream_router_ref;

            let setup_result: Result<_, RemoteProviderHostError> = async {
                let provider = build_provider_for_request(
                    backend.as_ref(),
                    &msg.provider,
                    &msg.model,
                    msg.params.as_ref(),
                )
                .await
                .map_err(|e| RemoteProviderHostError::Internal(e.to_string()))?;

                let tools_slice = msg.tools.as_deref();
                let stream = provider
                    .chat_stream_with_tools(&msg.messages, tools_slice)
                    .await
                    .map_err(|e| RemoteProviderHostError::ProviderChat {
                        operation: "chat_stream_with_tools".to_string(),
                        reason: serde_json::to_string(&e.to_payload())
                            .unwrap_or_else(|_| e.to_string()),
                    })?;

                Ok::<_, RemoteProviderHostError>((provider, stream))
            }
            .await;

            let (_provider, mut stream) = match setup_result {
                Ok(tuple) => tuple,
                Err(e) => {
                    let _ = stream_router_ref
                        .tell(&RoutedStreamRelayMessage {
                            request_id: cleanup_request_id.clone(),
                            message: StreamRelayMessage::ProviderError {
                                error: e.to_payload(),
                            },
                        })
                        .send();
                    remove_active_stream(&cleanup_streams, &cleanup_session_id, &cleanup_request_id);
                    return Ok(());
                }
            };

            tracing::Span::current().record("receiver_found", true);
            let relay_span = tracing::info_span!(
                "remote.provider_host.stream.relay",
                provider = %provider_name,
                model = %model,
                session_id = %session_id,
                request_id = %request_id,
                chunk_count = tracing::field::Empty,
                batch_count = tracing::field::Empty,
                max_batch_size = tracing::field::Empty,
            );
            tokio::spawn(
                async move {
                    let mut chunk_count = 0usize;
                    let mut batch_count = 0usize;
                    let mut max_batch_size = 0usize;
                    let relay_start = started_at;
                    let mut buffered: VecDeque<StreamRelayMessage> = VecDeque::new();
                    let mut pending_batch: Vec<StreamChunk> = Vec::new();
                    let mut disconnected_since: Option<tokio::time::Instant> = None;
                    let mut upstream_done = false;
                    let mut terminal_message: Option<StreamRelayMessage> = None;
                    let mut heartbeat_tick = tokio::time::interval(heartbeat_interval);
                    let mut unacked_batches: u32 = 0;
                    let mut last_ack_at = Instant::now();
                    heartbeat_tick.tick().await;

                    update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                        stream.phase = ProviderStreamPhase::WaitingFirstChunk;
                    });

                    let flush_batch = |buffered: &mut VecDeque<StreamRelayMessage>,
                                       pending_batch: &mut Vec<StreamChunk>,
                                       batch_count: &mut usize,
                                       max_batch_size: &mut usize| {
                        if pending_batch.is_empty() {
                            return;
                        }
                        *batch_count += 1;
                        *max_batch_size = (*max_batch_size).max(pending_batch.len());
                        let message = if pending_batch.len() == 1 {
                            StreamRelayMessage::Chunk(pending_batch.pop().expect("batch has one chunk"))
                        } else {
                            StreamRelayMessage::ChunkBatch(std::mem::take(pending_batch))
                        };
                        buffered.push_back(message);
                    };

                    'outer: loop {
                        tokio::select! {
                            _ = cancel_token.cancelled(), if !upstream_done => {
                                upstream_done = true;
                                flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                                update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                    stream.phase = ProviderStreamPhase::Cancelling;
                                    stream.last_error.get_or_insert_with(|| "cancelled".to_string());
                                });
                                terminal_message = Some(StreamRelayMessage::TransportFailed {
                                    error: querymt::error::LLMError::Transport {
                                        kind: querymt::error::TransportErrorKind::ConnectionClosed,
                                        message: "provider stream cancelled".to_string(),
                                    }.to_payload(),
                                });
                            }
                            _ = heartbeat_tick.tick() => {
                                let now = Instant::now();
                                let mut lease_expired = false;
                                let mut status = None;
                                update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                    if now >= stream.lease_expires_at {
                                        lease_expired = true;
                                        stream.phase = ProviderStreamPhase::LeaseExpired;
                                        stream.last_error = Some("stream lease expired".to_string());
                                        stream.cancel_token.cancel();
                                        return;
                                    }
                                    stream.last_heartbeat_at = now;
                                    status = Some(stream.status());
                                });
                                if lease_expired {
                                    continue;
                                }
                                if let Some(status) = status {
                                    buffered.push_back(StreamRelayMessage::Heartbeat {
                                        phase: status.phase,
                                        elapsed_ms: status.elapsed_ms,
                                        idle_ms: status.idle_ms,
                                        chunk_count: status.chunk_count,
                                    });
                                }
                            }
                            next_chunk = async {
                                if pending_batch.is_empty() {
                                    stream.next().await
                                } else {
                                    tokio::time::timeout(BATCH_FLUSH_INTERVAL, stream.next()).await.unwrap_or_default()
                                }
                            }, if !upstream_done => {
                                match next_chunk {
                                    Some(chunk_result) => {
                                        let mut finish_reason = None;
                                        match chunk_result {
                                            Ok(chunk) => {
                                                let chunk_is_done = matches!(chunk, StreamChunk::Done { .. });
                                                if let StreamChunk::Done { finish_reason: reason } = &chunk {
                                                    finish_reason = Some(*reason);
                                                    upstream_done = true;
                                                }
                                                pending_batch.push(chunk);
                                                if upstream_done || pending_batch.len() >= MAX_BATCH_SIZE {
                                                    flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                                                }
                                                let chunk_delta = if chunk_is_done { 0 } else { 1 };
                                                update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                                    stream.phase = ProviderStreamPhase::Streaming;
                                                    stream.last_progress_at = Instant::now();
                                                    stream.receiver_connected = disconnected_since.is_none();
                                                    stream.chunk_count = stream.chunk_count.saturating_add(chunk_delta as u64);
                                                });
                                            }
                                            Err(e) => {
                                                flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                                                upstream_done = true;
                                                let payload = e.to_payload();
                                                let message = e.to_string();
                                                update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                                    stream.phase = ProviderStreamPhase::Failed;
                                                    stream.last_error = Some(message);
                                                    stream.last_progress_at = Instant::now();
                                                });
                                                terminal_message = Some(StreamRelayMessage::ProviderError { error: payload });
                                            }
                                        }

                                        if finish_reason.is_some() {
                                            update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                                stream.phase = ProviderStreamPhase::Completed;
                                                stream.last_progress_at = Instant::now();
                                            });
                                        }
                                    }
                                    None => {
                                        if pending_batch.is_empty() {
                                            upstream_done = true;
                                            update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                                stream.phase = ProviderStreamPhase::Completed;
                                                stream.last_progress_at = Instant::now();
                                            });
                                        } else {
                                            flush_batch(&mut buffered, &mut pending_batch, &mut batch_count, &mut max_batch_size);
                                        }
                                    }
                                }
                            }
                        }

                        if let Some(message) = terminal_message.take() {
                            buffered.push_back(message);
                        }

                        loop {
                            if disconnected_since.is_some() {
                                let replay_count = buffered.iter().filter(|msg| keep_stream_message_buffered(msg)).count();
                                let reconnect_result = stream_router_ref.tell(&RoutedStreamRelayMessage {
                                    request_id: request_id.clone(),
                                    message: StreamRelayMessage::TransportReconnected {
                                        buffered_chunks: replay_count,
                                    },
                                }).send_ack().await;
                                if reconnect_result.is_ok() {
                                    let _disconnected_duration = disconnected_since.take().map(|s| s.elapsed());
                                    update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                        stream.phase = if stream.chunk_count == 0 {
                                            ProviderStreamPhase::WaitingFirstChunk
                                        } else {
                                            ProviderStreamPhase::Streaming
                                        };
                                        stream.receiver_connected = true;
                                    });
                                    unacked_batches = 0;
                                    last_ack_at = Instant::now();
                                } else {
                                    let since = disconnected_since.get_or_insert_with(tokio::time::Instant::now);
                                    update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                        stream.phase = ProviderStreamPhase::ReceiverDisconnected;
                                        stream.receiver_connected = false;
                                    });
                                    if since.elapsed() >= reconnect_grace {
                                        buffered.push_back(StreamRelayMessage::TransportFailed {
                                            error: querymt::error::LLMError::Transport {
                                                kind: querymt::error::TransportErrorKind::Timeout,
                                                message: format!("reconnect grace expired after {:?}", reconnect_grace),
                                            }.to_payload(),
                                        });
                                        update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                            stream.phase = ProviderStreamPhase::GraceExpired;
                                            stream.last_error = Some("reconnect grace expired".to_string());
                                        });
                                        upstream_done = true;
                                    }
                                    tokio::time::sleep(Duration::from_millis(250)).await;
                                    if upstream_done {
                                        break 'outer;
                                    }
                                    continue;
                                }
                            }

                            let Some(front) = buffered.front().cloned() else {
                                break;
                            };
                            let relay = RoutedStreamRelayMessage {
                                request_id: request_id.clone(),
                                message: front,
                            };
                            let is_terminal = relay_message_is_terminal(&relay.message);
                            let is_chunk_batch = matches!(
                                &relay.message,
                                StreamRelayMessage::Chunk(_) | StreamRelayMessage::ChunkBatch(_)
                            );
                            let should_ack = should_ack_relay_message(
                                &relay.message,
                                unacked_batches,
                                last_ack_at.elapsed(),
                                ACK_WINDOW_BATCHES,
                                ACK_WINDOW_INTERVAL,
                            );
                            let relay_result = if should_ack {
                                stream_router_ref.tell(&relay).send_ack().await.map_err(|e| e.to_string())
                            } else {
                                stream_router_ref.tell(&relay).send().map_err(|e| e.to_string())
                            };
                            if let Err(e) = relay_result {
                                if disconnected_since.is_none() {
                                    disconnected_since = Some(tokio::time::Instant::now());
                                    let _ = stream_router_ref.tell(&RoutedStreamRelayMessage {
                                        request_id: request_id.clone(),
                                        message: StreamRelayMessage::TransportDisconnected {
                                            reason: e.to_string(),
                                        },
                                    }).send();
                                    update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                        stream.phase = ProviderStreamPhase::ReceiverDisconnected;
                                        stream.receiver_connected = false;
                                        stream.last_error = Some(e.to_string());
                                    });
                                }
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                continue;
                            }

                            if is_chunk_batch {
                                if should_ack {
                                    unacked_batches = 0;
                                    last_ack_at = Instant::now();
                                } else {
                                    unacked_batches = unacked_batches.saturating_add(1);
                                }
                            } else {
                                unacked_batches = 0;
                                last_ack_at = Instant::now();
                            }

                            buffered.pop_front();
                            let relayed_chunks = match &relay.message {
                                StreamRelayMessage::Chunk(_) => 1,
                                StreamRelayMessage::ChunkBatch(chunks) => chunks.len(),
                                StreamRelayMessage::Heartbeat { .. }
                                | StreamRelayMessage::ProviderError { .. }
                                | StreamRelayMessage::TransportDisconnected { .. }
                                | StreamRelayMessage::TransportReconnected { .. }
                                | StreamRelayMessage::TransportFailed { .. } => 0,
                            };
                            chunk_count += relayed_chunks;
                            if relayed_chunks > 0 {
                                update_active_stream(&active_streams, &session_id, &request_id, |stream| {
                                    stream.chunk_count = chunk_count as u64;
                                    stream.last_progress_at = Instant::now();
                                });
                            }
                            let _elapsed_ms = relay_start.elapsed().as_millis();
                            if !keep_stream_message_buffered(&relay.message) {
                                continue;
                            }
                            if is_terminal {
                                break 'outer;
                            }
                        }

                        if upstream_done && buffered.is_empty() && pending_batch.is_empty() {
                            break;
                        }
                    }

                    remove_active_stream(&active_streams, &session_id, &request_id);
                    tracing::Span::current().record("chunk_count", chunk_count);
                    tracing::Span::current().record("batch_count", batch_count);
                    tracing::Span::current().record("max_batch_size", max_batch_size);
                }
                .instrument(relay_span),
            );

            Ok(())
        })
    }
}

impl Message<CancelProviderStreamRequest> for ProviderHostActor {
    type Reply = usize;

    async fn handle(
        &mut self,
        msg: CancelProviderStreamRequest,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.cancel_streams(
            &msg.session_id,
            msg.request_id.as_deref(),
            msg.reason.as_deref(),
        )
    }
}

impl Message<RenewProviderStreamLease> for ProviderHostActor {
    type Reply = bool;

    async fn handle(
        &mut self,
        msg: RenewProviderStreamLease,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.renew_stream_lease(&msg.session_id, &msg.request_id, msg.lease_ttl_secs)
    }
}

impl Message<GetProviderStreamStatus> for ProviderHostActor {
    type Reply = Option<ProviderStreamStatus>;

    async fn handle(
        &mut self,
        msg: GetProviderStreamStatus,
        _ctx: &mut Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.provider_stream_status(&msg.session_id, msg.request_id.as_deref())
    }
}

impl kameo::remote::RemoteActor for ProviderHostActor {
    const REMOTE_ID: &'static str = "querymt::ProviderHostActor";
}

#[_internal::linkme::distributed_slice(_internal::REMOTE_ACTORS)]
#[linkme(crate = _internal::linkme)]
static PROVIDER_HOST_ACTOR_REG: (&'static str, _internal::RemoteActorFns) = (
    <ProviderHostActor as kameo::remote::RemoteActor>::REMOTE_ID,
    _internal::RemoteActorFns {
        link: (|actor_id, sibling_id, sibling_remote_id| {
            Box::pin(_internal::link::<ProviderHostActor>(
                actor_id,
                sibling_id,
                sibling_remote_id,
            ))
        }) as _internal::RemoteLinkFn,
        unlink: (|actor_id, sibling_id| {
            Box::pin(_internal::unlink::<ProviderHostActor>(actor_id, sibling_id))
        }) as _internal::RemoteUnlinkFn,
        signal_link_died: (|dead_actor_id, notified_actor_id, stop_reason| {
            Box::pin(_internal::signal_link_died::<ProviderHostActor>(
                dead_actor_id,
                notified_actor_id,
                stop_reason,
            ))
        }) as _internal::RemoteSignalLinkDiedFn,
    },
);

macro_rules! remote_provider_msg_impl {
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

remote_provider_msg_impl!(
    ProviderHostActor,
    ProviderChatRequest,
    "querymt::ProviderChatRequest",
    REG_PROVIDER_CHAT_REQUEST
);
remote_provider_msg_impl!(
    ProviderHostActor,
    ProviderStreamRequest<kameo::actor::RemoteActorRef<crate::ProviderStreamRouterActor>>,
    "querymt::ProviderStreamRequest",
    REG_PROVIDER_STREAM_REQUEST
);
remote_provider_msg_impl!(
    ProviderHostActor,
    CancelProviderStreamRequest,
    "querymt::CancelProviderStreamRequest",
    REG_CANCEL_PROVIDER_STREAM_REQUEST
);
remote_provider_msg_impl!(
    ProviderHostActor,
    RenewProviderStreamLease,
    "querymt::RenewProviderStreamLease",
    REG_RENEW_PROVIDER_STREAM_LEASE
);
remote_provider_msg_impl!(
    ProviderHostActor,
    GetProviderStreamStatus,
    "querymt::GetProviderStreamStatus",
    REG_GET_PROVIDER_STREAM_STATUS
);
