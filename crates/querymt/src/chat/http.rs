use crate::{
    chat::{ChatCall, ChatCapabilities, ChatOutput, StreamChunk},
    error::{LLMError, classify_status_only},
};
use http::{Request, Response};
use std::{future::Future, pin::Pin};

/// Parses semantic stream frames supplied by the HTTP adapter.
///
/// The adapter buffers arbitrary transport chunks through the next newline, so
/// implementations receive complete SSE lines rather than raw network chunks.
pub trait ChatStreamParser: Send {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError>;

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LLMError> {
        Ok(Vec::new())
    }
}

/// An HTTP request and its provider-private decoding context. The adapter must
/// retain context locally, never transmit it, and return it only to the decoder
/// for this request. Dropping a cancelled request drops its context as well.
pub struct PreparedChatRequest {
    pub request: Request<Vec<u8>>,
    pub context: serde_json::Value,
}

impl PreparedChatRequest {
    /// A request whose response needs no provider-private decoding context.
    #[must_use]
    pub fn stateless(request: Request<Vec<u8>>) -> Self {
        Self {
            request,
            context: serde_json::Value::Null,
        }
    }
}

pub trait HTTPChatProvider: Send + Sync {
    /// Builds the HTTP request for `call`.
    ///
    /// One function serves streaming and non-streaming requests (`stream`).
    /// Set `call.options` override the provider's configured values for this
    /// request only; a call with empty options must produce the same request
    /// as before options existed.
    fn prepare_chat_request(
        &self,
        call: &ChatCall<'_>,
        stream: bool,
    ) -> Result<PreparedChatRequest, LLMError>;

    /// Parse a **successful** chat HTTP body.
    ///
    /// `context` is the opaque value returned by
    /// [`Self::prepare_chat_request`] for this request. The HTTP adapter only
    /// calls this after a success status. Non-success responses go through
    /// [`Self::classify_chat_error`] instead — do not re-check status here.
    fn parse_chat(
        &self,
        resp: Response<Vec<u8>>,
        context: serde_json::Value,
    ) -> Result<ChatOutput, LLMError>;

    /// Transfer the request's opaque context into its independent stream
    /// parser. Providers that report [`ChatCapabilities::streaming`] must
    /// override this.
    fn chat_stream_parser(
        &self,
        context: serde_json::Value,
    ) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        let _ = context;
        Err(LLMError::NotImplemented(
            "Streaming not supported by this HTTP provider".into(),
        ))
    }

    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_status_only(
            response.status().as_u16(),
            response.headers(),
            response.body(),
        )
    }

    fn classify_chat_error_async<'a>(
        &'a self,
        response: &'a Response<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = LLMError> + Send + 'a>> {
        Box::pin(async move { self.classify_chat_error(response) })
    }

    /// What this provider supports. Defaults to nothing.
    fn capabilities(&self) -> ChatCapabilities {
        ChatCapabilities::NONE
    }
}
