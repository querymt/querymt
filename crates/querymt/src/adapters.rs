use crate::{
    chat::{ChatMessage, ChatProvider, ChatResponse, StreamChunk},
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    outbound::{call_outbound, call_outbound_stream},
    HTTPLLMProvider, LLMProvider, Tool,
};
use async_trait::async_trait;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use tracing::instrument;

pub struct LLMProviderFromHTTP {
    inner: Arc<dyn HTTPLLMProvider>,
}

impl LLMProviderFromHTTP {
    pub fn new(inner: Arc<dyn HTTPLLMProvider>) -> Self {
        Self { inner }
    }

    async fn do_chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        let req = self
            .inner
            .chat_request(messages, tools)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        self.inner.parse_chat(resp)
    }
}

#[async_trait]
impl ChatProvider for LLMProviderFromHTTP {
    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    #[instrument(name = "http_adapter.chat_with_tools", skip_all)]
    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        self.do_chat(messages, tools).await
    }

    #[instrument(name = "http_adapter.chat_stream_with_tools", skip_all)]
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

        let req = self
            .inner
            .chat_request(messages, tools)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let stream = call_outbound_stream(req)
            .await
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let inner = self.inner.clone();
        let s = stream
            .map(move |res: reqwest::Result<bytes::Bytes>| {
                res.map_err(|e: reqwest::Error| LLMError::HttpError(e.to_string()))
            })
            .chain(futures::stream::once(futures::future::ready(Ok(
                bytes::Bytes::from_static(b"\n"),
            ))))
            .scan(Vec::new(), move |buffer, res| {
                let inner = inner.clone();
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
                                match inner.parse_chat_stream_chunk(line) {
                                    Ok(mut parsed_chunks) => {
                                        chunks.append(&mut parsed_chunks);
                                    }
                                    Err(e) => {
                                        log::debug!(
                                            "Failed to parse SSE line: {:?}, error: {}",
                                            String::from_utf8_lossy(line),
                                            e
                                        );
                                    }
                                }
                                start = i + 1;
                            }
                        }
                        *buffer = buffer[start..].to_vec();
                        Ok(chunks)
                    }
                    Err(e) => Err(e),
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
    #[instrument(name = "http_adapter.embed", skip_all)]
    async fn embed(&self, inputs: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
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
    #[instrument(name = "http_adapter.complete", skip_all)]
    async fn complete(&self, req_obj: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        let req = self.inner.complete_request(req_obj)?;
        let resp = call_outbound(req)
            .await
            .map_err(|e| LLMError::HttpError(format!("{:#}", e)))?;
        self.inner
            .parse_complete(resp)
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))
    }
}

impl LLMProvider for LLMProviderFromHTTP {
    fn tools(&self) -> Option<&[Tool]> {
        self.inner.tools()
    }
}
