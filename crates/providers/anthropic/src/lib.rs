//! Anthropic API client implementation for chat and completion functionality.
//!
//! This module provides integration with Anthropic's Claude models through their API.

use std::collections::HashMap;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::{header::CONTENT_TYPE, Method, Request, Response};
use querymt::{
    chat::{
        http::HTTPChatProvider, ChatMessage, ChatResponse, ChatRole, MessageType, Tool, ToolChoice,
    },
    completion::{http::HTTPCompletionProvider, CompletionRequest, CompletionResponse},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    get_env_var,
    providers::{ModelPricing, ProvidersRegistry},
    FunctionCall, HTTPLLMProvider, ToolCall, Usage,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// Client for interacting with Anthropic's API.
///
/// Provides methods for chat and completion requests using Anthropic's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Anthropic {
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout_seconds: Option<u64>,
    pub system: Option<String>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning: Option<bool>,
    pub thinking_budget_tokens: Option<u32>,
}

/// Anthropic-specific tool format that matches their API structure
#[derive(Serialize, Debug)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(rename = "input_schema")]
    schema: &'a serde_json::Value,
}

/// Configuration for the thinking feature
#[derive(Serialize, Debug)]
struct ThinkingConfig {
    #[serde(rename = "type")]
    thinking_type: String,
    budget_tokens: u32,
}

/// Request payload for Anthropic's messages API endpoint.
#[derive(Serialize, Debug)]
struct AnthropicCompleteRequest<'a> {
    messages: Vec<AnthropicMessage<'a>>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
}

/// Individual message in an Anthropic chat conversation.
#[derive(Serialize, Debug)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: Vec<MessageContent<'a>>,
}

#[derive(Serialize, Debug)]
struct MessageContent<'a> {
    #[serde(rename = "type")]
    message_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<ImageUrlContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<ImageSource<'a>>,
    // tool use
    #[serde(skip_serializing_if = "Option::is_none", rename = "id")]
    tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "name")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "input")]
    tool_input: Option<Value>,
    // tool result
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_use_id")]
    tool_result_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "content")]
    tool_output: Option<String>,
}

#[derive(Serialize, Debug)]
struct ImageUrlContent<'a> {
    url: &'a str,
}

#[derive(Serialize, Debug)]
struct ImageSource<'a> {
    #[serde(rename = "type")]
    source_type: &'a str,
    media_type: &'a str,
    data: String,
}

/// Response from Anthropic's messages API endpoint.
#[derive(Deserialize, Debug)]
struct AnthropicCompleteResponse {
    content: Vec<AnthropicContent>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Debug)]
struct AnthropicStreamResponse {
    #[serde(rename = "type")]
    response_type: String,
    /// Index of the content block (for content_block_start, content_block_delta, content_block_stop)
    index: Option<usize>,
    /// Content block for content_block_start events
    content_block: Option<AnthropicStreamContentBlock>,
    /// Delta for content_block_delta and message_delta events
    delta: Option<AnthropicDelta>,
}

/// Content block within an Anthropic streaming content_block_start event.
#[derive(Deserialize, Debug)]
struct AnthropicStreamContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    /// Tool use ID (for tool_use blocks)
    id: Option<String>,
    /// Tool name (for tool_use blocks)
    name: Option<String>,
    /// Initial text (for text blocks, usually empty)
    #[allow(dead_code)]
    text: Option<String>,
}

/// Delta content within an Anthropic streaming response.
#[derive(Deserialize, Debug)]
struct AnthropicDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    /// Text content (for text_delta)
    text: Option<String>,
    /// Partial JSON string (for input_json_delta)
    partial_json: Option<String>,
    /// Thinking content (for thinking_delta)
    thinking: Option<String>,
    /// Stop reason (for message_delta)
    stop_reason: Option<String>,
}

/// Content block within an Anthropic API response.
#[derive(Serialize, Deserialize, Debug)]
struct AnthropicContent {
    text: Option<String>,
    #[serde(rename = "type")]
    content_type: Option<String>,
    thinking: Option<String>,
    name: Option<String>,
    input: Option<serde_json::Value>,
    id: Option<String>,
}

impl std::fmt::Display for AnthropicCompleteResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for content in self.content.iter() {
            match content.content_type {
                Some(ref t) if t == "tool_use" => write!(
                    f,
                    "{{\n \"name\": {}, \"input\": {}\n}}",
                    content.name.clone().unwrap_or_default(),
                    content
                        .input
                        .clone()
                        .unwrap_or(serde_json::Value::Null)
                        .to_string()
                )?,
                Some(ref t) if t == "thinking" => {
                    write!(f, "{}", content.thinking.clone().unwrap_or_default())?
                }
                _ => write!(
                    f,
                    "{}",
                    self.content
                        .iter()
                        .map(|c| c.text.clone().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n")
                )?,
            }
        }
        Ok(())
    }
}

impl ChatResponse for AnthropicCompleteResponse {
    fn text(&self) -> Option<String> {
        Some(
            self.content
                .iter()
                .filter_map(|c| {
                    if c.content_type == Some("text".to_string()) || c.content_type.is_none() {
                        c.text.clone()
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn thinking(&self) -> Option<String> {
        self.content
            .iter()
            .find(|c| c.content_type == Some("thinking".to_string()))
            .and_then(|c| c.thinking.clone())
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        match self
            .content
            .iter()
            .filter_map(|c| {
                if c.content_type == Some("tool_use".to_string()) {
                    Some(ToolCall {
                        id: c.id.clone().unwrap_or_default(),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: c.name.clone().unwrap_or_default(),
                            arguments: serde_json::to_string(
                                &c.input.clone().unwrap_or(serde_json::Value::Null),
                            )
                            .unwrap_or_default(),
                        },
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<ToolCall>>()
        {
            v if v.is_empty() => None,
            v => Some(v),
        }
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
}

impl Anthropic {
    fn default_base_url() -> Url {
        Url::parse("https://api.anthropic.com/v1/").unwrap()
    }
}

impl HTTPChatProvider for Anthropic {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        if self.api_key.is_empty() {
            return Err(LLMError::AuthError("Missing Anthropic API key".to_string()));
        }

        let anthropic_messages: Vec<AnthropicMessage> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| AnthropicMessage {
                role: match m.role {
                    ChatRole::User => "user",
                    ChatRole::Assistant => "assistant",
                },
                content: match &m.message_type {
                    MessageType::Text => vec![MessageContent {
                        message_type: Some("text"),
                        text: Some(&m.content),
                        image_url: None,
                        source: None,
                        tool_use_id: None,
                        tool_input: None,
                        tool_name: None,
                        tool_result_id: None,
                        tool_output: None,
                    }],
                    MessageType::Pdf(raw_bytes) => {
                        vec![MessageContent {
                            message_type: Some("document"),
                            text: None,
                            image_url: None,
                            source: Some(ImageSource {
                                source_type: "base64",
                                media_type: "application/pdf",
                                data: BASE64.encode(raw_bytes),
                            }),
                            tool_use_id: None,
                            tool_input: None,
                            tool_name: None,
                            tool_result_id: None,
                            tool_output: None,
                        }]
                    }
                    MessageType::Image((image_mime, raw_bytes)) => {
                        vec![MessageContent {
                            message_type: Some("image"),
                            text: None,
                            image_url: None,
                            source: Some(ImageSource {
                                source_type: "base64",
                                media_type: image_mime.mime_type(),
                                data: BASE64.encode(raw_bytes),
                            }),
                            tool_use_id: None,
                            tool_input: None,
                            tool_name: None,
                            tool_result_id: None,
                            tool_output: None,
                        }]
                    }
                    MessageType::ImageURL(ref url) => vec![MessageContent {
                        message_type: Some("image_url"),
                        text: None,
                        image_url: Some(ImageUrlContent { url }),
                        source: None,
                        tool_use_id: None,
                        tool_input: None,
                        tool_name: None,
                        tool_result_id: None,
                        tool_output: None,
                    }],
                    MessageType::ToolUse(calls) => {
                        let mut content = Vec::new();
                        if !m.content.is_empty() {
                            content.push(MessageContent {
                                message_type: Some("text"),
                                text: Some(&m.content),
                                image_url: None,
                                source: None,
                                tool_use_id: None,
                                tool_input: None,
                                tool_name: None,
                                tool_result_id: None,
                                tool_output: None,
                            });
                        }
                        content.extend(calls.iter().map(|c| {
                            MessageContent {
                                message_type: Some("tool_use"),
                                text: None,
                                image_url: None,
                                source: None,
                                tool_use_id: Some(c.id.clone()),
                                tool_input: Some(
                                    serde_json::from_str(&c.function.arguments)
                                        .unwrap_or_else(|_| serde_json::json!({})),
                                ),
                                tool_name: Some(c.function.name.clone()),
                                tool_result_id: None,
                                tool_output: None,
                            }
                        }));
                        content
                    }
                    MessageType::ToolResult(responses) => responses
                        .iter()
                        .map(|r| MessageContent {
                            message_type: Some("tool_result"),
                            text: None,
                            image_url: None,
                            source: None,
                            tool_use_id: None,
                            tool_input: None,
                            tool_name: None,
                            tool_result_id: Some(r.id.clone()),
                            tool_output: Some(r.function.arguments.clone()),
                        })
                        .collect(),
                },
            })
            .collect();

        let maybe_tool_slice: Option<&[Tool]> = tools.or(self.tools.as_deref());
        let anthropic_tools = maybe_tool_slice.map(|slice| {
            slice
                .iter()
                .map(|tool| AnthropicTool {
                    name: &tool.function.name,
                    description: &tool.function.description,
                    schema: &tool.function.parameters,
                })
                .collect::<Vec<_>>()
        });

        let tool_choice = match self.tool_choice {
            Some(ToolChoice::Auto) => {
                Some(HashMap::from([("type".to_string(), "auto".to_string())]))
            }
            Some(ToolChoice::Any) => Some(HashMap::from([("type".to_string(), "any".to_string())])),
            Some(ToolChoice::Tool(ref tool_name)) => Some(HashMap::from([
                ("type".to_string(), "tool".to_string()),
                ("name".to_string(), tool_name.clone()),
            ])),
            Some(ToolChoice::None) => {
                Some(HashMap::from([("type".to_string(), "none".to_string())]))
            }
            None => None,
        };

        let final_tool_choice = if anthropic_tools.is_some() {
            tool_choice.clone()
        } else {
            None
        };

        let thinking = if self.reasoning.unwrap_or(false) {
            Some(ThinkingConfig {
                thinking_type: "enabled".to_string(),
                budget_tokens: self.thinking_budget_tokens.unwrap_or(16000),
            })
        } else {
            None
        };

        let req_body = AnthropicCompleteRequest {
            messages: anthropic_messages,
            model: &self.model,
            max_tokens: Some(self.max_tokens),
            temperature: Some(if self.reasoning.unwrap_or(false) {
                // NOTE: Ignoring temperature when reasoning is enabled. Temperature in this cases
                // should always be set to `1.0`.
                1.0
            } else {
                self.temperature
            }),
            system: self.system.as_deref(),
            stream: self.stream,
            top_p: self.top_p,
            top_k: self.top_k,
            tools: anthropic_tools,
            tool_choice: final_tool_choice,
            thinking,
        };

        let json_req = serde_json::to_vec(&req_body)?;
        let url = Anthropic::default_base_url().join("messages")?;
        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .body(json_req)?)
    }

    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        if !resp.status().is_success() {
            let status = resp.status();
            let error_text: String = "".to_string();
            return Err(LLMError::ResponseFormatError {
                message: format!("API returned error status: {}", status),
                raw_response: error_text,
            });
        }

        let json_resp: AnthropicCompleteResponse = serde_json::from_slice(resp.body())
            .map_err(|e| LLMError::HttpError(format!("Failed to parse JSON: {}", e)))?;

        Ok(Box::new(json_resp))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn parse_chat_stream_chunk(
        &self,
        chunk: &[u8],
    ) -> Result<Vec<querymt::chat::StreamChunk>, LLMError> {
        let text = std::str::from_utf8(chunk).map_err(|e| LLMError::GenericError(e.to_string()))?;
        let mut chunks = Vec::new();

        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data: ") {
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }

                let stream_resp: AnthropicStreamResponse =
                    serde_json::from_str(data).map_err(|e| LLMError::ResponseFormatError {
                        message: format!("Failed to parse Anthropic stream data: {}", e),
                        raw_response: data.to_string(),
                    })?;

                match stream_resp.response_type.as_str() {
                    "content_block_start" => {
                        if let (Some(index), Some(block)) =
                            (stream_resp.index, stream_resp.content_block)
                        {
                            if block.block_type == "tool_use" {
                                chunks.push(querymt::chat::StreamChunk::ToolUseStart {
                                    index,
                                    id: block.id.unwrap_or_default(),
                                    name: block.name.unwrap_or_default(),
                                });
                            }
                        }
                    }
                    "content_block_delta" => {
                        if let (Some(index), Some(delta)) = (stream_resp.index, stream_resp.delta) {
                            if let Some(text) = delta.text {
                                chunks.push(querymt::chat::StreamChunk::Text(text));
                            } else if let Some(thinking) = delta.thinking {
                                chunks.push(querymt::chat::StreamChunk::Text(thinking));
                            } else if let Some(partial_json) = delta.partial_json {
                                chunks.push(querymt::chat::StreamChunk::ToolUseInputDelta {
                                    index,
                                    partial_json,
                                });
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(delta) = stream_resp.delta {
                            if let Some(stop_reason) = delta.stop_reason {
                                chunks.push(querymt::chat::StreamChunk::Done { stop_reason });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(chunks)
    }
}

impl HTTPCompletionProvider for Anthropic {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        unimplemented!()
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        unimplemented!()
    }
}

impl HTTPEmbeddingProvider for Anthropic {
    fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::ProviderError(
            "Embedding not supported".to_string(),
        ))
    }

    fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::ProviderError(
            "Embedding not supported".to_string(),
        ))
    }
}

impl HTTPLLMProvider for Anthropic {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

#[warn(dead_code)]
fn get_pricing(model: &str) -> Option<ModelPricing> {
    if let Some(models) = get_env_var!("PROVIDERS_REGISTRY_DATA") {
        if let Ok(registry) = serde_json::from_str::<ProvidersRegistry>(&models) {
            return registry.get_pricing("anthropic", model).cloned();
        }
    }
    None
}

mod factory;
