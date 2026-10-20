use crate::{
    Tool,
    chat::{ChatMessage, ChatOutput, StreamChunk},
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

pub trait HTTPChatProvider: Send + Sync {
    fn prepare_chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
        stream: bool,
    ) -> Result<PreparedChatRequest, LLMError> {
        let request = if stream {
            self.chat_stream_request(messages, tools)?
        } else {
            self.chat_request(messages, tools)?
        };
        Ok(PreparedChatRequest {
            request,
            context: serde_json::Value::Null,
        })
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

    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError>;

    fn chat_stream_request(
        &self,
        _messages: &[ChatMessage],
        _tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::NotImplemented(
            "Streaming request construction not supported by this HTTP provider".into(),
        ))
    }

    /// Parse a **successful** chat HTTP body.
    ///
    /// The HTTP adapter only calls this after a success status. Non-success
    /// responses go through [`Self::classify_chat_error`] instead — do not
    /// re-check status here.
    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<ChatOutput, LLMError>;

    /// Decode using the opaque context returned during request preparation.
    fn parse_chat_with_context(
        &self,
        resp: Response<Vec<u8>>,
        _context: serde_json::Value,
    ) -> Result<ChatOutput, LLMError> {
        self.parse_chat(resp)
    }

    /// Transfer the request's opaque context into its independent stream parser.
    fn chat_stream_parser_with_context(
        &self,
        _context: serde_json::Value,
    ) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        self.chat_stream_parser()
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Err(LLMError::NotImplemented(
            "Streaming not supported by this HTTP provider".into(),
        ))
    }
}
