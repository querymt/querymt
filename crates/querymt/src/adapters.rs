use crate::{
    HTTPLLMProvider, LLMProvider, Tool,
    chat::{ChatMessage, ChatProvider, ChatResponse, StreamChunk},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    outbound::{call_outbound, call_outbound_stream},
    stt, tts,
};
use async_trait::async_trait;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(feature = "tracing")]
use tracing::instrument;

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

    async fn do_chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.ensure_credential_fresh().await?;

        let req = self
            .inner
            .chat_request(messages, tools)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let resp = call_outbound(req).await?;

        self.inner.parse_chat(resp)
    }
}

#[async_trait]
impl ChatProvider for LLMProviderFromHTTP {
    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.chat_with_tools", skip_all)
    )]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.do_chat(messages, tools).await
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.chat_stream_with_tools", skip_all)
    )]
    async fn chat_stream_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamChunk, LLMError>> + Send>>, LLMError>
    {
        if !self.inner.supports_streaming() {
            return Err(LLMError::NotImplemented(
                "Streaming not supported by underlying HTTP provider".into(),
            ));
        }

        self.ensure_credential_fresh().await?;

        let req = self
            .inner
            .chat_stream_request(messages, tools)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let stream = call_outbound_stream(req).await?;
        let mut parser = self
            .inner
            .chat_stream_parser()
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

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
                                Err(e) => return futures::future::ready(Some(Err(e))),
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
        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::HttpError(format!("{:#}", e)))?;
        self.inner
            .parse_embed(resp)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
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
        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::HttpError(format!("{:#}", e)))?;
        self.inner
            .parse_complete(resp)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
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
        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::HttpError(format!("{:#}", e)))?;
        self.inner
            .parse_stt(resp)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
    }

    #[cfg_attr(
        feature = "tracing",
        instrument(name = "http_adapter.speech", skip_all)
    )]
    async fn speech(&self, req_obj: &tts::TtsRequest) -> Result<tts::TtsResponse, LLMError> {
        self.ensure_credential_fresh().await?;
        let req = self.inner.tts_request(req_obj)?;
        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::HttpError(format!("{:#}", e)))?;
        self.inner
            .parse_tts(resp)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyResolver, static_key};
    use crate::chat::http::{ChatStreamParser, HTTPChatProvider};
    use crate::completion::http::HTTPCompletionProvider;
    use crate::embedding::http::HTTPEmbeddingProvider;
    use http::{Request, Response};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DummyHttpProvider {
        resolver: Option<Arc<dyn ApiKeyResolver>>,
    }

    #[derive(Debug)]
    struct CountingResolver {
        resolves: AtomicUsize,
    }

    impl CountingResolver {
        fn new() -> Self {
            Self {
                resolves: AtomicUsize::new(0),
            }
        }

        fn resolve_count(&self) -> usize {
            self.resolves.load(Ordering::SeqCst)
        }
    }

    impl ApiKeyResolver for CountingResolver {
        fn resolve(&self) -> Pin<Box<dyn Future<Output = Result<(), LLMError>> + Send + '_>> {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn current(&self) -> String {
            if self.resolve_count() > 0 {
                "resolved-token".to_string()
            } else {
                "stale-token".to_string()
            }
        }
    }

    struct ResolveAwareHttpProvider {
        resolver: Arc<dyn ApiKeyResolver>,
    }

    impl HTTPChatProvider for DummyHttpProvider {
        fn chat_request(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<Request<Vec<u8>>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }

        fn parse_chat(&self, _resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }
    }

    impl HTTPCompletionProvider for DummyHttpProvider {
        fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }

        fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }
    }

    impl HTTPEmbeddingProvider for DummyHttpProvider {
        fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }

        fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }
    }

    impl HTTPLLMProvider for DummyHttpProvider {
        fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
            self.resolver.as_ref()
        }

        fn set_key_resolver(&mut self, resolver: Arc<dyn ApiKeyResolver>) {
            self.resolver = Some(resolver);
        }
    }

    impl HTTPChatProvider for ResolveAwareHttpProvider {
        fn chat_request(
            &self,
            _messages: &[ChatMessage],
            _tools: Option<&[Tool]>,
        ) -> Result<Request<Vec<u8>>, LLMError> {
            let token = self.resolver.current();
            let req = Request::builder()
                .method("POST")
                .uri("https://example.invalid/chat")
                .header("authorization", format!("Bearer {token}"))
                .body(Vec::new())
                .map_err(|e| LLMError::InvalidRequest(format!("failed building request: {e}")))?;
            Ok(req)
        }

        fn parse_chat(&self, _resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }
    }

    impl HTTPCompletionProvider for ResolveAwareHttpProvider {
        fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }

        fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }
    }

    impl HTTPEmbeddingProvider for ResolveAwareHttpProvider {
        fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }

        fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
            Err(LLMError::NotImplemented("unused in test".into()))
        }
    }

    impl HTTPLLMProvider for ResolveAwareHttpProvider {
        fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
            Some(&self.resolver)
        }
    }

    #[test]
    fn set_key_resolver_forwards_to_inner_provider() {
        let inner: Box<dyn HTTPLLMProvider> = Box::new(DummyHttpProvider { resolver: None });
        let mut adapter = LLMProviderFromHTTP::new(inner);
        let resolver = static_key("resolver-token");

        adapter.set_key_resolver(resolver.clone());

        let forwarded = adapter
            .key_resolver()
            .expect("resolver should be set on wrapped provider");
        assert_eq!(forwarded.current(), "resolver-token");
    }

    #[tokio::test]
    async fn ensure_credential_fresh_resolves_before_request_building() {
        let resolver = Arc::new(CountingResolver::new());
        let inner: Box<dyn HTTPLLMProvider> = Box::new(ResolveAwareHttpProvider {
            resolver: resolver.clone(),
        });
        let adapter = LLMProviderFromHTTP::new(inner);

        assert_eq!(resolver.resolve_count(), 0);
        assert_eq!(
            adapter
                .inner
                .chat_request(&[], None)
                .expect("request should build")
                .headers()
                .get("authorization")
                .expect("auth header should exist"),
            "Bearer stale-token"
        );

        adapter
            .ensure_credential_fresh()
            .await
            .expect("resolver should succeed");

        assert_eq!(resolver.resolve_count(), 1);
        assert_eq!(
            adapter
                .inner
                .chat_request(&[], None)
                .expect("request should build")
                .headers()
                .get("authorization")
                .expect("auth header should exist"),
            "Bearer resolved-token"
        );
    }
}
