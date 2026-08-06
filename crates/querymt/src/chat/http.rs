use crate::{
    Tool,
    chat::{ChatMessage, ChatResponse, StreamChunk},
    error::{LLMError, ProviderDecodeError, classify_status_only},
};
use http::{Request, Response};

pub trait ChatStreamParser: Send {
    /// Decode one SSE/frame chunk. Return unattributed decode errors — the HTTP
    /// adapter stamps [`crate::HTTPLLMProvider::provider_name`] once.
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, ProviderDecodeError>;

    fn finish(&mut self) -> Result<Vec<StreamChunk>, ProviderDecodeError> {
        Ok(Vec::new())
    }
}

pub trait HTTPChatProvider: Send + Sync {
    /// Classify a non-success HTTP chat response **without** provider identity.
    /// The HTTP adapter calls [`ProviderDecodeError::attribute`] with
    /// [`crate::HTTPLLMProvider::provider_name`].
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> ProviderDecodeError {
        classify_status_only(
            response.status().as_u16(),
            response.headers(),
            response.body(),
        )
        .into()
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
