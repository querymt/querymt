use querymt::chat::{ChatResponse, FinishReason};
use querymt::Usage;
use std::fmt;

/// Response from a llama.cpp chat completion.
#[derive(Debug)]
pub(crate) struct LlamaCppChatResponse {
    pub(crate) text: String,
    pub(crate) thinking: Option<String>,
    pub(crate) tool_calls: Option<Vec<querymt::ToolCall>>,
    pub(crate) finish_reason: FinishReason,
    pub(crate) usage: Usage,
}

impl fmt::Display for LlamaCppChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.text)
    }
}

impl ChatResponse for LlamaCppChatResponse {
    fn text(&self) -> Option<String> {
        Some(self.text.clone())
    }

    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }

    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        self.tool_calls.clone()
    }

    fn usage(&self) -> Option<Usage> {
        Some(self.usage.clone())
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        Some(self.finish_reason)
    }
}

/// Generated text from a completion request.
pub(crate) struct GeneratedText {
    pub(crate) text: String,
    pub(crate) usage: Usage,
}
