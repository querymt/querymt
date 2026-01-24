use crate::{
    chat::{ChatMessage, ChatResponse, FinishReason, Tool},
    completion::CompletionRequest,
    stt, ToolCall, Usage,
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

#[allow(dead_code)]
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

#[derive(Deserialize, Serialize)]
pub struct ExtismSttRequest<C> {
    pub cfg: C,
    pub audio_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

impl<C> ExtismSttRequest<C> {
    pub fn into_stt_request(self) -> Result<stt::SttRequest, crate::error::LLMError> {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

        let audio = BASE64
            .decode(self.audio_base64)
            .map_err(|e| crate::error::LLMError::InvalidRequest(e.to_string()))?;

        Ok(stt::SttRequest {
            audio,
            filename: self.filename,
            mime_type: self.mime_type,
            model: self.model,
            language: self.language,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtismSttResponse {
    pub text: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ExtismChatResponse {
    pub text: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub thinking: Option<String>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<FinishReason>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExtismChatChunk {
    pub chunk: crate::chat::StreamChunk,
    pub usage: Option<Usage>,
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
    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
    fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason
    }
}

impl From<Box<dyn ChatResponse>> for ExtismChatResponse {
    fn from(r: Box<dyn ChatResponse>) -> Self {
        ExtismChatResponse {
            text: r.text(),
            tool_calls: r.tool_calls(),
            thinking: r.thinking(),
            usage: r.usage(),
            finish_reason: r.finish_reason(),
        }
    }
}
