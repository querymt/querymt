use crate::{
    HTTPLLMProvider, LLMProvider, Tool,
    chat::{ChatCall, ChatCapabilities, ChatOutput, ChatProvider, ChatStream, StreamChunk},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    outbound::{OutboundStreamResult, call_outbound, call_outbound_raw, call_outbound_stream_raw},
    stt, tts,
};
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
#[cfg(feature = "tracing")]
use tracing::{Instrument, instrument};

/// Await `$fut` inside a child span named `$name` when tracing is enabled.
///
/// The streaming request is split into credential refresh, request building and
/// the HTTP send so a slow first byte can be attributed to one of them.
macro_rules! phase {
    ($name:literal, $fut:expr) => {{
        #[cfg(feature = "tracing")]
        {
            $fut.instrument(tracing::info_span!($name)).await
        }
        #[cfg(not(feature = "tracing"))]
        {
            $fut.await
        }
    }};
}

pub struct LLMProviderFromHTTP {
    inner: Arc<dyn HTTPLLMProvider>,
}

impl LLMProviderFromHTTP {
    pub fn new(inner: Box<dyn HTTPLLMProvider>) -> Self {
        Self {
            inner: Arc::from(inner),
        }
    }

    // WASI request builders/parsers are synchronous and cannot run on a Tokio
    // worker. Keep HTTP execution async; offload only the short transformations.
    async fn speech_blocking<T: Send + 'static>(
        &self,
        operation: &'static str,
        call: impl FnOnce(&dyn HTTPLLMProvider) -> Result<T, LLMError> + Send + 'static,
    ) -> Result<T, LLMError> {
        let inner = self.inner.clone();
        #[cfg(feature = "tracing")]
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            #[cfg(feature = "tracing")]
            let _guard = span.enter();
            call(inner.as_ref())
        })
        .await
        .map_err(|error| {
            LLMError::ProviderError(format!("HTTP {operation} task failed: {error}"))
        })?
    }

    /// Ensure the provider's credential is fresh before building a request.
    ///
    /// If the provider has an [`ApiKeyResolver`](crate::auth::ApiKeyResolver),
    /// this calls `resolve()` so that subsequent sync calls to `current()`
    /// in the provider's request builders return a valid credential.
    async fn ensure_credential_fresh(&self) -> Result<(), LLMError> {
        if let Some(resolver) = self.inner.key_resolver() {
            resolver.resolve().await?;
        }
        Ok(())
    }
}

#[async_trait]
impl ChatProvider for LLMProviderFromHTTP {
    fn capabilities(&self) -> ChatCapabilities {
        self.inner.capabilities()
    }

    #[cfg_attr(feature = "tracing", instrument(name = "http_adapter.chat", skip_all))]
    async fn chat(&self, call: &ChatCall<'_>) -> Result<ChatOutput, LLMError> {
        self.ensure_credential_fresh().await?;

        let prepared = self.inner.prepare_chat_request(call, false)?;

        let resp = call_outbound_raw(prepared.request).await?;
        if !resp.status().is_success() {
            return Err(self.inner.classify_chat_error_async(&resp).await);
        }

        self.inner.parse_chat(resp, prepared.context)
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.chat_stream", skip_all)
    )]
    async fn chat_stream(&self, call: &ChatCall<'_>) -> Result<ChatStream, LLMError> {
        if !self.inner.capabilities().streaming {
            return Err(LLMError::NotImplemented(
                "Streaming not supported by underlying HTTP provider".into(),
            ));
        }

        phase!(
            "http_adapter.credential_refresh",
            self.ensure_credential_fresh()
        )?;

        let prepared = {
            #[cfg(feature = "tracing")]
            let span = tracing::info_span!(
                "http_adapter.prepare_request",
                http.request.body.size = tracing::field::Empty
            );
            #[cfg(feature = "tracing")]
            let _entered = span.enter();
            let prepared = self.inner.prepare_chat_request(call, true)?;
            #[cfg(feature = "tracing")]
            span.record(
                "http.request.body.size",
                prepared.request.body().len() as u64,
            );
            prepared
        };

        // Covers the network round trip up to response headers; the body is
        // consumed later by the caller, so this is effectively time to first byte.
        let stream = match phase!(
            "http_adapter.send",
            call_outbound_stream_raw(prepared.request)
        )? {
            OutboundStreamResult::Failure { response } => {
                return Err(self.inner.classify_chat_error_async(&response).await);
            }
            OutboundStreamResult::Success { stream } => stream,
        };

        let mut parser = self.inner.chat_stream_parser(prepared.context)?;
        let s = stream
            .map(move |res: reqwest::Result<bytes::Bytes>| res.map_err(LLMError::from))
            .chain(futures::stream::iter([
                Ok(bytes::Bytes::from_static(b"\n")),
                Ok(bytes::Bytes::new()),
            ]))
            .scan((Vec::new(), false), move |(buffer, done), res| {
                if *done {
                    return futures::future::ready(None);
                }

                let res = match res {
                    Ok(bytes) => {
                        if !bytes.is_empty() {
                            log::trace!("Received chunk: {} bytes", bytes.len());
                        }
                        buffer.extend_from_slice(&bytes);
                        let mut chunks = Vec::new();
                        let mut start = 0;
                        for i in 0..buffer.len() {
                            if buffer[i] == b'\n' {
                                let line = &buffer[start..i + 1];
                                match parser.parse_chunk(line) {
                                    Ok(mut parsed_chunks) => {
                                        chunks.append(&mut parsed_chunks);
                                    }
                                    Err(e) => {
                                        log::debug!(
                                            "Failed to parse SSE line: {:?}, error: {}",
                                            String::from_utf8_lossy(line),
                                            e
                                        );
                                        *done = true;
                                        return futures::future::ready(Some(Err(e)));
                                    }
                                }
                                start = i + 1;
                            }
                        }
                        *buffer = buffer[start..].to_vec();

                        if bytes.is_empty() {
                            *done = true;
                            match parser.finish() {
                                Ok(mut tail) => chunks.append(&mut tail),
                                Err(e) => {
                                    return futures::future::ready(Some(Err(e)));
                                }
                            }
                        }

                        Ok(chunks)
                    }
                    Err(e) => {
                        *done = true;
                        Err(e)
                    }
                };
                futures::future::ready(Some(res))
            })
            .flat_map(|res: Result<Vec<StreamChunk>, LLMError>| {
                let v: Vec<Result<StreamChunk, LLMError>> = match res {
                    Ok(chunks) => chunks.into_iter().map(Ok).collect(),
                    Err(e) => vec![Err(e)],
                };
                futures::stream::iter(v)
            });

        Ok(Box::pin(s))
    }
}

#[async_trait]
impl EmbeddingProvider for LLMProviderFromHTTP {
    #[cfg_attr(feature = "tracing", instrument(name = "http_adapter.embed", skip_all))]
    async fn embed(&self, inputs: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        self.ensure_credential_fresh().await?;
        let req = self.inner.embed_request(&inputs)?;
        // call_outbound already classifies non-success statuses.
        let resp = call_outbound(req).await?;
        self.inner.parse_embed(resp)
    }
}

#[async_trait]
impl CompletionProvider for LLMProviderFromHTTP {
    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.complete", skip_all)
    )]
    async fn complete(&self, req_obj: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        self.ensure_credential_fresh().await?;
        let req = self.inner.complete_request(req_obj)?;
        let resp = call_outbound(req).await?;
        self.inner.parse_complete(resp)
    }
}

#[async_trait]
impl LLMProvider for LLMProviderFromHTTP {
    fn tools(&self) -> Option<&[Tool]> {
        self.inner.tools()
    }

    fn set_key_resolver(&mut self, resolver: Arc<dyn crate::auth::ApiKeyResolver>) {
        Arc::get_mut(&mut self.inner)
            .expect("configure the key resolver before issuing HTTP requests")
            .set_key_resolver(resolver);
    }

    fn key_resolver(&self) -> Option<&Arc<dyn crate::auth::ApiKeyResolver>> {
        self.inner.key_resolver()
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.transcribe", skip_all)
    )]
    async fn transcribe(&self, req_obj: &stt::SttRequest) -> Result<stt::SttResponse, LLMError> {
        self.ensure_credential_fresh().await?;
        let req_obj = req_obj.clone();
        let req = self
            .speech_blocking("stt_request", move |inner| inner.stt_request(&req_obj))
            .await?;
        let resp = call_outbound(req).await?;
        self.speech_blocking("parse_stt", move |inner| inner.parse_stt(resp))
            .await
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.speech", skip_all)
    )]
    async fn speech(&self, req_obj: &tts::TtsRequest) -> Result<tts::TtsResponse, LLMError> {
        self.ensure_credential_fresh().await?;
        let req_obj = req_obj.clone();
        let req = self
            .speech_blocking("tts_request", move |inner| inner.tts_request(&req_obj))
            .await?;
        let resp = call_outbound(req).await?;
        self.speech_blocking("parse_tts", move |inner| inner.parse_tts(resp))
            .await
    }
}

#[cfg(test)]
#[path = "adapters/tests.rs"]
mod tests;
