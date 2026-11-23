use crate::{RemoteProviderClientCore, RemoteProviderClientTransport, RemoteProviderStreamState};
use querymt::LLMProvider;
use querymt::chat::{
    ChatCall, ChatCapabilities, ChatOutput, ChatOutputProvenance, ChatProvider, ChatStream,
};
use querymt::completion::{CompletionProvider, CompletionRequest, CompletionResponse};
use querymt::embedding::EmbeddingProvider;
use querymt::error::LLMError;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone)]
pub struct RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    core: RemoteProviderClientCore<TTransport>,
}

impl<TTransport> RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    pub fn new(core: RemoteProviderClientCore<TTransport>) -> Self {
        Self { core }
    }

    pub fn core(&self) -> &RemoteProviderClientCore<TTransport> {
        &self.core
    }

    fn remote_session_id(&self) -> Option<&str> {
        self.core.config().remote_session_id()
    }

    pub async fn cancel_remote_stream(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        reason: Option<&str>,
    ) -> Result<(), LLMError> {
        self.core
            .cancel_stream(session_id, request_id, reason)
            .await
    }

    pub async fn get_remote_stream_status(
        &self,
        session_id: &str,
        request_id: Option<&str>,
    ) -> Option<crate::ProviderStreamStatus> {
        self.core.get_stream_status(session_id, request_id).await
    }
}

#[async_trait::async_trait]
impl<TTransport> ChatProvider for RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    fn capabilities(&self) -> ChatCapabilities {
        self.core.capabilities()
    }

    /// A remote provider cannot say where its peer's output comes from: the
    /// provider contract negotiates capabilities, not provenance. Callers keep
    /// to what stored output records (`None`).
    fn output_provenance(&self) -> Option<ChatOutputProvenance> {
        None
    }

    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        let provider_name = self.core.config().provider_name.clone();
        let model_name = self.core.config().model.clone();
        let target_locator = self.core.config().target_locator.clone();
        let result = self
            .core
            .chat(call, |error| {
                matches!(
                    error,
                    LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::ConnectionClosed
                            | querymt::error::TransportErrorKind::ConnectionRefused,
                        ..
                    }
                )
            })
            .await;
        let chat_response = match result {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(
                    target: "querymt_remote::provider::chat",
                    provider = %provider_name,
                    model = %model_name,
                    target_locator = %target_locator,
                    error = %error,
                    error_payload = %serde_json::to_string(&error.to_payload())
                        .unwrap_or_else(|_| "unavailable".to_string()),
                    "remote provider chat request failed"
                );
                return Err(error);
            }
        };

        Ok(chat_response.into_canonical_output())
    }

    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        let request_id = Uuid::now_v7().to_string();
        let session_id = self
            .remote_session_id()
            .unwrap_or("unknown-session")
            .to_string();
        let provider_name = self.core.config().provider_name.clone();
        let model_name = self.core.config().model.clone();
        let target_locator = self.core.config().target_locator.clone();
        let setup_span = tracing::info_span!(
            "remote.provider_client.stream.setup",
            session_id = %session_id,
            request_id = %request_id,
            provider = %provider_name,
            model = %model_name,
            target_locator = %target_locator,
            message_count = call.messages.len(),
            tool_count = call.tools.map(|t| t.len()).unwrap_or(0),
            first_chunk_ms = tracing::field::Empty,
        );
        let _setup_guard = setup_span.enter();

        tracing::info!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            provider = %provider_name,
            model = %model_name,
            target_locator = %target_locator,
            "starting remote provider stream setup"
        );

        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "looking up remote provider host"
        );
        let host_ref = self.core.lookup_host().await?;
        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "remote provider host lookup completed"
        );

        let (tx, rx) = mpsc::unbounded_channel::<crate::StreamRelayMessage>();
        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "registering and attaching stream router consumer"
        );
        let (_router_ref, remote_router_ref) = self
            .core
            .prepare_stream_router(&host_ref, &session_id, &request_id, tx)
            .await?;
        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "stream router consumer attached"
        );

        let stream_request = self.core.build_stream_request(
            call,
            session_id.clone(),
            request_id.clone(),
            remote_router_ref,
            self.core.transport().stream_reconnect_grace().as_secs(),
        );
        let host_ref = self
            .core
            .validate_or_refresh_host(host_ref, stream_request.item_aware_contract_version)
            .await?;

        tracing::debug!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            "sending remote provider stream request"
        );
        let host_ref = match self
            .core
            .send_stream_request_with_retry(&host_ref, stream_request, |error| {
                matches!(
                    error,
                    LLMError::Transport {
                        kind: querymt::error::TransportErrorKind::ConnectionClosed
                            | querymt::error::TransportErrorKind::ConnectionRefused,
                        ..
                    }
                )
            })
            .await
        {
            Ok(host) => host,
            Err(error) => {
                tracing::warn!(
                    target: "querymt_remote::provider::stream",
                    session_id = %session_id,
                    request_id = %request_id,
                    provider = %provider_name,
                    model = %model_name,
                    target_locator = %target_locator,
                    error = %error,
                    error_payload = %serde_json::to_string(&error.to_payload())
                        .unwrap_or_else(|_| "unavailable".to_string()),
                    "remote provider stream request setup failed"
                );
                return Err(error);
            }
        };
        tracing::info!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            provider = %provider_name,
            model = %model_name,
            target_locator = %target_locator,
            "remote provider stream request acknowledged"
        );

        let raw_stream = rx;
        let session_id_for_stream = session_id.clone();
        let request_id_for_stream = request_id.clone();
        let provider_for_stream = provider_name.clone();
        let model_for_stream = model_name.clone();
        let target_for_stream = target_locator.clone();
        let reconnect_grace = self.core.transport().stream_reconnect_grace();
        let stream_start = Instant::now();
        let local_peer_id = self.core.transport().local_peer_id_display().await;
        let target_peer_id_display = self
            .core
            .transport()
            .target_peer_id_display(self.core.config().target_locator())
            .await;
        let lease_renew_every = self.core.config().lease_renew_every();
        let renew_lease_factory =
            self.core
                .make_renew_lease_fn(host_ref.clone(), session_id.clone(), request_id.clone());
        let peer_alive_fn = self.core.make_target_peer_alive_fn();
        let setup_span = setup_span.clone();
        tracing::info!(
            target: "querymt_remote::provider::stream",
            session_id = %session_id,
            request_id = %request_id,
            local_peer_id = %local_peer_id,
            target_peer_id = %target_peer_id_display,
            lease_renew_every_ms = lease_renew_every.as_millis(),
            reconnect_grace_ms = reconnect_grace.as_millis(),
            "remote provider stream consumer ready"
        );

        let relay_timeout = crate::provider_host_actor::STREAM_RELAY_TIMEOUT;
        // Authorization and the first send can each consume a relay timeout
        // before the host detects a disconnect and starts its own grace.
        let lease_loss_grace = reconnect_grace
            .max(Duration::from_secs(1))
            .saturating_add(relay_timeout.saturating_mul(2));
        let stream = futures_util::stream::unfold(
            Some((
                raw_stream,
                RemoteProviderStreamState::new(),
                tokio::time::Instant::now() + lease_renew_every,
                None::<tokio::time::Instant>,
                None::<crate::provider_client_runtime::RenewLeaseFuture>,
            )),
            move |state| {
                let session_id_for_stream = session_id_for_stream.clone();
                let request_id_for_stream = request_id_for_stream.clone();
                let target_peer_id_display = target_peer_id_display.clone();
                let local_peer_id = local_peer_id.clone();
                let provider_for_stream = provider_for_stream.clone();
                let model_for_stream = model_for_stream.clone();
                let target_for_stream = target_for_stream.clone();
                let setup_span = setup_span.clone();
                let renew_lease = renew_lease_factory.clone();
                let peer_alive_fn = peer_alive_fn.clone();
                async move {
                    let (
                        mut raw_stream,
                        mut stream_state,
                        mut renew_due,
                        mut lease_lost_at,
                        mut renewal,
                    ) = state?;
                    if stream_state.is_finished() {
                        tracing::debug!(
                            target: "querymt_remote::provider::stream",
                            session_id = %session_id_for_stream,
                            request_id = %request_id_for_stream,
                            chunk_index = stream_state.chunk_index(),
                            "remote provider stream finished after terminal chunk"
                        );
                        return None;
                    }

                    if let Some((chunk, chunk_index, first_chunk_ms)) =
                        stream_state.take_pending(stream_start)
                    {
                        if let Some(first_chunk_ms) = first_chunk_ms {
                            setup_span.record("first_chunk_ms", first_chunk_ms);
                        }
                        if querymt::chat::chunk_is_terminal(&chunk) {
                            tracing::info!(
                                target: "querymt_remote::provider::stream",
                                session_id = %session_id_for_stream,
                                request_id = %request_id_for_stream,
                                local_peer_id = %local_peer_id,
                                target_peer_id = %target_peer_id_display,
                                provider = %provider_for_stream,
                                model = %model_for_stream,
                                target_node = %target_for_stream,
                                chunk_index,
                                elapsed_ms = stream_start.elapsed().as_millis(),
                                chunk = ?chunk,
                                pending_chunks = stream_state.pending_chunks_len(),
                                "stream terminal received from remote provider pending batch"
                            );
                        }
                        return Some((
                            Ok(chunk),
                            Some((raw_stream, stream_state, renew_due, lease_lost_at, renewal)),
                        ));
                    }

                    loop {
                        let now = tokio::time::Instant::now();
                        if renewal.is_none() && now >= renew_due {
                            let renew = renew_lease();
                            renewal = Some(Box::pin(async move {
                                tokio::time::timeout(lease_renew_every, renew)
                                    .await
                                    .unwrap_or(false)
                            }));
                        }
                        let reconnect = stream_state.reconnect_remaining(reconnect_grace);
                        // The host may unregister before its terminal is delivered.
                        // A positive renewal clears this deadline; callbacks alone do not.
                        let lost =
                            lease_lost_at.map(|at| lease_loss_grace.saturating_sub(at.elapsed()));
                        let next = tokio::select! {
                            biased;
                            _ = sleep_if_set(lost) => {
                                // Drain only what was already queued, in order. A terminal
                                // received while this consumer was idle still takes precedence.
                                raw_stream.close();
                                match raw_stream.try_recv() {
                                    Ok(message) => Some(message),
                                    Err(_) => return Some((Err(LLMError::Transport {
                                        kind: querymt::error::TransportErrorKind::ConnectionClosed,
                                        message: "remote provider stream lease could not be renewed and no terminal arrived".into(),
                                    }), None)),
                                }
                            },
                            _ = sleep_if_set(reconnect) => return Some((
                                Err(RemoteProviderClientCore::<TTransport>::reconnect_timeout_error(reconnect_grace)), None,
                            )),
                            renewed = async { renewal.as_mut().expect("renewal branch requires a future").await }, if renewal.is_some() => {
                                let now = tokio::time::Instant::now();
                                if renewed {
                                    lease_lost_at = None;
                                } else {
                                    lease_lost_at.get_or_insert(now);
                                }
                                renewal = None;
                                renew_due = now + if renewed { lease_renew_every } else { lease_renew_every.min(lease_loss_grace / 2) };
                                continue;
                            }
                            item = raw_stream.recv() => item,
                            _ = tokio::time::sleep_until(renew_due), if renewal.is_none() => continue,
                        };

                        let peer_alive = next.is_some() || peer_alive_fn().await;
                        match RemoteProviderClientCore::<TTransport>::poll_stream_message(
                            crate::PollStreamContext {
                                setup_span: setup_span.clone(),
                                stream_start,
                                session_id: &session_id_for_stream,
                                request_id: &request_id_for_stream,
                                local_peer_id: &local_peer_id,
                                target_peer_id: &target_peer_id_display,
                                provider_name: &provider_for_stream,
                                model: &model_for_stream,
                                target_name: &target_for_stream,
                            },
                            next,
                            &mut stream_state,
                            peer_alive,
                        ) {
                            Ok(Some(chunk)) => {
                                break Some((
                                    Ok(chunk),
                                    Some((
                                        raw_stream,
                                        stream_state,
                                        renew_due,
                                        lease_lost_at,
                                        renewal,
                                    )),
                                ));
                            }
                            Ok(None) => {
                                continue;
                            }
                            Err(error) => {
                                break Some((Err(error), None));
                            }
                        }
                    }
                }
            },
        );

        Ok(Box::pin(stream))
    }
}

async fn sleep_if_set(duration: Option<Duration>) {
    match duration {
        Some(duration) if !duration.is_zero() => tokio::time::sleep(duration).await,
        Some(_) => {}
        None => std::future::pending().await,
    }
}

#[async_trait::async_trait]
impl<TTransport> CompletionProvider for RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    async fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(
            "RemoteChatProvider: completion not supported (use chat instead)".into(),
        ))
    }
}

#[async_trait::async_trait]
impl<TTransport> EmbeddingProvider for RemoteChatProvider<TTransport>
where
    TTransport: RemoteProviderClientTransport + 'static,
{
    async fn embed(&self, _input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "RemoteChatProvider: embedding not supported".into(),
        ))
    }
}

impl<TTransport> LLMProvider for RemoteChatProvider<TTransport> where
    TTransport: RemoteProviderClientTransport + 'static
{
}
