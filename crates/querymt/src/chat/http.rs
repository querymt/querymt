use crate::{
    chat::{ChatMessage, ChatResponse},
    error::LLMError,
    Tool,
};
use http::{Request, Response};
use std::error::Error;

pub trait HTTPChatProvider: Send + Sync {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError>;
    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, Box<dyn Error>>;
}
