use crate::{
    Tool,
    chat::{ChatMessage, ChatResponse, StreamChunk},
    error::{LLMError, classify_status_only},
};
use http::{Request, Response};
use std::{future::Future, pin::Pin};

pub trait ChatStreamParser: Send {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError>;

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LLMError> {
        Ok(Vec::new())
    }
}

pub trait HTTPChatProvider: Send + Sync {
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
    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError>;

    fn supports_streaming(&self) -> bool {
        false
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Err(LLMError::NotImplemented(
            "Streaming not supported by this HTTP provider".into(),
        ))
    }
}
