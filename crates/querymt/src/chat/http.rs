use crate::{
    Tool,
    chat::{ChatMessage, ChatResponse, StreamChunk},
    error::{LLMError, classify_http_status},
};
use http::{Request, Response};

pub trait ChatStreamParser: Send {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError>;

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LLMError> {
        Ok(Vec::new())
    }
}

pub trait HTTPChatProvider: Send + Sync {
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_http_status(
            response.status().as_u16(),
            response.headers(),
            response.body(),
        )
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
