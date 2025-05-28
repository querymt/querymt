use crate::{
    chat::{ChatMessage, ChatResponse, Tool},
    completion::CompletionRequest,
    ToolCall,
};
use serde::{Deserialize, Serialize};
use std::fmt;

pub trait BinaryCodec {
    type Bytes: AsRef<[u8]>;
    type Error;

    fn to_bytes(&self) -> Result<Self::Bytes, Self::Error>;
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Error>
    where
        Self: Sized;
}

pub trait FromBytesOwned: Sized {
    type Error;

    fn from_bytes_owned(bytes: &[u8]) -> Result<Self, Self::Error>;
}

#[derive(Deserialize, Serialize)]
pub struct ExtismChatRequest<C> {
    pub cfg: C,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<Tool>>,
}

#[derive(Serialize, Deserialize)]
pub struct ExtismEmbedRequest<C> {
    pub cfg: C,
    pub inputs: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct ExtismCompleteRequest<C> {
    pub cfg: C,
    pub req: CompletionRequest,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExtismChatResponse {
    pub text: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
}

impl fmt::Display for ExtismChatResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // If there’s a top‐level `text`, show that…
        if let Some(ref txt) = self.text {
            write!(f, "{}", txt)
        } else {
            // …otherwise Fall back to Debug or JSON:
            write!(f, "{:?}", self)
        }
    }
}

impl ChatResponse for ExtismChatResponse {
    fn text(&self) -> Option<String> {
        self.text.clone()
    }
    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        self.tool_calls.clone()
    }
    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }
}

impl From<Box<dyn ChatResponse>> for ExtismChatResponse {
    fn from(r: Box<dyn ChatResponse>) -> Self {
        ExtismChatResponse {
            text: r.text(),
            tool_calls: r.tool_calls(),
            thinking: r.thinking(),
        }
    }
}
