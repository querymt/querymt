use super::*;
use crate::auth::{ApiKeyResolver, static_key};
use crate::chat::ChatProvider;
use crate::chat::http::{ChatStreamParser, HTTPChatProvider};
use crate::completion::http::HTTPCompletionProvider;
use crate::embedding::http::HTTPEmbeddingProvider;
use crate::error::LLMError;
use futures::StreamExt;
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

    fn parse_chat(&self, _resp: Response<Vec<u8>>) -> Result<ChatOutput, LLMError> {
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

    fn parse_chat(&self, _resp: Response<Vec<u8>>) -> Result<ChatOutput, LLMError> {
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

/// One-shot HTTP/1.1 responder for adapter integration tests.
async fn serve_once(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
    serve_chunks_once(status_line, headers, vec![body.to_vec()]).await
}

async fn serve_chunks_once(
    status_line: &str,
    headers: &[(&str, &str)],
    chunks: Vec<Vec<u8>>,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let status_line = status_line.to_owned();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let mut response = format!("{status_line}\r\nTransfer-Encoding: chunked\r\n");
        for (k, v) in &headers {
            response.push_str(&format!("{k}: {v}\r\n"));
        }
        response.push_str("Connection: close\r\n\r\n");
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write hdr");
        for chunk in chunks {
            socket
                .write_all(format!("{:X}\r\n", chunk.len()).as_bytes())
                .await
                .expect("write chunk size");
            socket.write_all(&chunk).await.expect("write chunk");
            socket.write_all(b"\r\n").await.expect("write chunk end");
            socket.flush().await.expect("flush chunk");
            tokio::task::yield_now().await;
        }
        socket.write_all(b"0\r\n\r\n").await.expect("finish body");
    });

    format!("http://{addr}/chat")
}

/// Streaming HTTP provider that hits a fixed URI and classifies with a
/// vendor-shaped kind so tests can prove the adapter path (not status-only).
struct StreamTestProvider {
    uri: String,
    parser: StreamTestParserMode,
}

#[derive(Clone, Copy)]
enum StreamTestParserMode {
    /// Never reached on open-failure tests.
    Unused,
    /// `finish()` returns a classified permanent failure.
    FinishClassified,
    /// Non-empty lines classify as rate-limited.
    ChunkClassified,
    /// Parses complete SSE lines handed off by the HTTP adapter framing layer.
    SemanticSse,
}

struct StreamTestParser {
    mode: StreamTestParserMode,
}

impl ChatStreamParser for StreamTestParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, crate::error::LLMError> {
        match self.mode {
            StreamTestParserMode::ChunkClassified
                if !chunk.iter().all(|b| b.is_ascii_whitespace()) =>
            {
                Err(crate::error::ProviderFailure::new(
                    crate::error::ProviderErrorKind::RateLimited,
                    "mid-stream boom",
                )
                .with_retry_after_secs(Some(3))
                .into())
            }
            StreamTestParserMode::SemanticSse => {
                let line = std::str::from_utf8(chunk)
                    .map_err(|error| LLMError::ResponseFormatError {
                        message: format!("invalid UTF-8 at parser boundary: {error}"),
                        raw_response: String::from_utf8_lossy(chunk).into_owned(),
                    })?
                    .trim();
                let Some(data) = line.strip_prefix("data: ") else {
                    return Ok(Vec::new());
                };
                if data == "[DONE]" {
                    return Ok(vec![StreamChunk::Done {
                        finish_reason: crate::chat::FinishReason::Stop,
                    }]);
                }
                let value: serde_json::Value = serde_json::from_str(data)?;
                Ok(value["text"]
                    .as_str()
                    .map(|text| vec![StreamChunk::Text(text.to_owned())])
                    .unwrap_or_default())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, crate::error::LLMError> {
        match self.mode {
            StreamTestParserMode::FinishClassified => Err(crate::error::ProviderFailure::new(
                crate::error::ProviderErrorKind::QuotaExceeded,
                "finish boom",
            )
            .into()),
            _ => Ok(Vec::new()),
        }
    }
}

impl HTTPChatProvider for StreamTestProvider {
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> crate::error::LLMError {
        // Prove the adapter calls the provider classifier (not only status-only).
        let body = String::from_utf8_lossy(response.body());
        crate::error::ProviderFailure::new(
            crate::error::ProviderErrorKind::RateLimited,
            format!("classified:{body}"),
        )
        .with_retry_after_secs(Some(9))
        .with_code(Some("vendor_rate".into()))
        .into()
    }

    fn chat_request(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::NotImplemented("unused".into()))
    }

    fn chat_stream_request(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        Request::builder()
            .method("POST")
            .uri(&self.uri)
            .body(Vec::new())
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))
    }

    fn parse_chat(&self, _resp: Response<Vec<u8>>) -> Result<ChatOutput, LLMError> {
        Err(LLMError::NotImplemented("unused".into()))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(StreamTestParser { mode: self.parser }))
    }
}

impl HTTPCompletionProvider for StreamTestProvider {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::NotImplemented("unused".into()))
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented("unused".into()))
    }
}

impl HTTPEmbeddingProvider for StreamTestProvider {
    fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::NotImplemented("unused".into()))
    }

    fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented("unused".into()))
    }
}

impl HTTPLLMProvider for StreamTestProvider {}

#[tokio::test]
async fn stream_open_non_success_preserves_classification() {
    let uri = serve_once(
        "HTTP/1.1 429 Too Many Requests",
        &[("Retry-After", "5"), ("Content-Type", "application/json")],
        br#"{"error":"vendor body"}"#,
    )
    .await;

    let inner: Box<dyn HTTPLLMProvider> = Box::new(StreamTestProvider {
        uri,
        parser: StreamTestParserMode::Unused,
    });
    let adapter = LLMProviderFromHTTP::new(inner);

    let err = match adapter.chat_stream_with_tools(&[], None).await {
        Ok(_) => panic!("non-success stream open must fail before yielding a stream"),
        Err(e) => e,
    };

    match &err {
        LLMError::ProviderResponseError(failure) => {
            assert!(
                failure.message().contains("classified:")
                    && failure.message().contains("vendor body"),
                "adapter must use provider classify_chat_error, got message={}",
                failure.message()
            );
            assert_eq!(failure.kind(), crate::error::ProviderErrorKind::RateLimited);
            assert_eq!(failure.code(), Some("vendor_rate"));
            assert_eq!(failure.retry_after_secs(), Some(9));
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
    assert!(err.is_retryable());
    assert!(err.is_rate_limited());
}

#[tokio::test]
async fn stream_parser_finish_failure_preserves_classification() {
    let uri = serve_once(
        "HTTP/1.1 200 OK",
        &[("Content-Type", "text/event-stream")],
        b"", // empty body; adapter still calls finish() on stream end
    )
    .await;

    let inner: Box<dyn HTTPLLMProvider> = Box::new(StreamTestProvider {
        uri,
        parser: StreamTestParserMode::FinishClassified,
    });
    let adapter = LLMProviderFromHTTP::new(inner);

    let mut stream = adapter
        .chat_stream_with_tools(&[], None)
        .await
        .expect("200 open should succeed");

    let err = loop {
        match stream.next().await {
            Some(Err(e)) => break e,
            Some(Ok(_)) => continue,
            None => panic!("expected classified finish error before stream end"),
        }
    };

    match &err {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.message(), "finish boom");
            assert_eq!(
                failure.kind(),
                crate::error::ProviderErrorKind::QuotaExceeded
            );
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn stream_parser_chunk_failure_preserves_classification() {
    let uri = serve_once(
        "HTTP/1.1 200 OK",
        &[("Content-Type", "text/event-stream")],
        b"data: nope\n",
    )
    .await;

    let inner: Box<dyn HTTPLLMProvider> = Box::new(StreamTestProvider {
        uri,
        parser: StreamTestParserMode::ChunkClassified,
    });
    let adapter = LLMProviderFromHTTP::new(inner);

    let mut stream = adapter
        .chat_stream_with_tools(&[], None)
        .await
        .expect("200 open should succeed");

    let err = loop {
        match stream.next().await {
            Some(Err(e)) => break e,
            Some(Ok(_)) => continue,
            None => panic!("expected classified chunk error before stream end"),
        }
    };

    match &err {
        LLMError::ProviderResponseError(failure) => {
            assert_eq!(failure.message(), "mid-stream boom");
            assert_eq!(failure.kind(), crate::error::ProviderErrorKind::RateLimited);
            assert_eq!(failure.retry_after_secs(), Some(3));
        }
        other => panic!("expected ProviderResponseError, got {other}"),
    }
    assert!(err.is_retryable());
}

#[tokio::test]
async fn http_adapter_frames_split_utf8_json_and_sse_once() {
    let body = "data: {\"text\":\"héllo 🌍\"}\n\ndata: [DONE]\n\n".as_bytes();
    let emoji = body
        .windows("🌍".len())
        .position(|window| window == "🌍".as_bytes())
        .expect("emoji bytes");
    let json_split = body
        .windows("héllo".len())
        .position(|window| window == "héllo".as_bytes())
        .expect("text bytes")
        + 2;
    let sse_split = body
        .windows(b"data: [DONE]".len())
        .position(|window| window == b"data: [DONE]")
        .expect("done frame")
        + 3;
    let mut boundaries = vec![
        1,
        json_split,
        emoji + 1,
        emoji + 3,
        sse_split,
        body.len() - 1,
    ];
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut start = 0;
    let mut chunks = Vec::new();
    for end in boundaries.into_iter().chain(std::iter::once(body.len())) {
        if end > start {
            chunks.push(body[start..end].to_vec());
            start = end;
        }
    }

    let uri = serve_chunks_once(
        "HTTP/1.1 200 OK",
        &[("Content-Type", "text/event-stream")],
        chunks,
    )
    .await;
    let adapter = LLMProviderFromHTTP::new(Box::new(StreamTestProvider {
        uri,
        parser: StreamTestParserMode::SemanticSse,
    }));
    let events: Vec<StreamChunk> = adapter
        .chat_stream_with_tools(&[], None)
        .await
        .expect("stream open")
        .map(|result| result.expect("semantic event"))
        .collect()
        .await;

    assert_eq!(events.len(), 2, "SSE framing must not duplicate events");
    assert!(matches!(&events[0], StreamChunk::Text(text) if text == "héllo 🌍"));
    assert!(matches!(
        events[1],
        StreamChunk::Done {
            finish_reason: crate::chat::FinishReason::Stop
        }
    ));
}
