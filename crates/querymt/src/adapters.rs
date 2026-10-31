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
    inner: Box<dyn HTTPLLMProvider>,
}

impl LLMProviderFromHTTP {
    pub fn new(inner: Box<dyn HTTPLLMProvider>) -> Self {
        Self { inner }
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
        let resp = call_outbound_raw(req).await?;
        if !resp.status().is_success() {
            return Err(self.inner.classify_embed_error(&resp));
        }
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
        self.inner.set_key_resolver(resolver);
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
        let req = self.inner.stt_request(req_obj)?;
        let resp = call_outbound(req).await?;
        self.inner.parse_stt(resp)
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.speech", skip_all)
    )]
    async fn speech(&self, req_obj: &tts::TtsRequest) -> Result<tts::TtsResponse, LLMError> {
        self.ensure_credential_fresh().await?;
        let req = self.inner.tts_request(req_obj)?;
        let resp = call_outbound(req).await?;
        self.inner.parse_tts(resp)
    }
}

#[cfg(test)]
#[path = "adapters/tests.rs"]
mod tests;
