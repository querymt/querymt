//! Anthropic API client implementation for chat and completion functionality.
//!
//! This module provides integration with Anthropic's Claude models through their API.

use std::collections::HashMap;

use regex::Regex;

/// Tool name prefix used for OAuth requests to avoid conflicts with server-side tools
const TOOL_PREFIX: &str = "mcp_";

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT},
};
use querymt::{
    FunctionCall, HTTPLLMProvider, ToolCall, Usage,
    chat::{
        ChatMessage, ChatResponse, ChatRole, FinishReason, MessageType, Tool, ToolChoice,
        http::HTTPChatProvider,
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    get_env_var, handle_http_error,
    providers::{ModelPricing, ProvidersRegistry},
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use url::Url;

/// Authentication type for Anthropic API
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    /// Standard API key authentication (x-api-key header)
    #[serde(rename = "api_key")]
    ApiKey,
    /// OAuth token authentication (Authorization: Bearer header)
    #[serde(rename = "oauth")]
    OAuth,
}

/// Determines the authentication type based on an explicit setting or by auto-detecting from the API key format.
///
/// If `explicit_auth_type` is `Some`, returns that value directly.
/// Otherwise, auto-detects based on the token prefix:
/// - OAuth tokens: `sk-ant-oat<digits>-...` (e.g., sk-ant-oat01-...)
/// - API keys: `sk-ant-api<digits>-...` (e.g., sk-ant-api03-...)
///
/// If the token format is unrecognized, logs a warning and defaults to API key authentication.
pub fn detect_auth_type(api_key: &str, explicit_auth_type: Option<AuthType>) -> AuthType {
    if let Some(auth_type) = explicit_auth_type {
        return auth_type;
    }

    // Check for OAuth token pattern: sk-ant-oat<digits>-
    if api_key.starts_with("sk-ant-oat") {
        if let Some(rest) = api_key.strip_prefix("sk-ant-oat")
            && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            return AuthType::OAuth;
        }
    }

    // Check for API key pattern: sk-ant-api<digits>-
    if api_key.starts_with("sk-ant-api") {
        if let Some(rest) = api_key.strip_prefix("sk-ant-api")
            && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            return AuthType::ApiKey;
        }
    }

    // Fallback: Check for generic sk-ant- prefix (backward compatibility)
    if api_key.starts_with("sk-ant-") {
        eprintln!(
            "Warning: Anthropic token format not recognized (expected 'sk-ant-oat<N>-' or 'sk-ant-api<N>-'). \
            Defaulting to API key authentication. Consider setting 'auth_type' explicitly."
        );
        return AuthType::ApiKey;
    }

    // Token doesn't match Anthropic format at all
    eprintln!(
        "Warning: Token does not match expected Anthropic format (should start with 'sk-ant-'). \
        Defaulting to API key authentication. This may cause authentication failures."
    );
    AuthType::ApiKey
}

/// Client for interacting with Anthropic's API.
///
/// Provides methods for chat and completion requests using Anthropic's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Anthropic {
    pub api_key: String,
    /// Optional: Explicitly specify authentication type.
    /// If not provided, will auto-detect based on api_key format:
    /// - OAuth tokens: `sk-ant-oat<digits>-...` (e.g., sk-ant-oat01-...)
    /// - API keys: `sk-ant-api<digits>-...` (e.g., sk-ant-api03-...)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<AuthType>,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub timeout_seconds: Option<u64>,
    pub system: Option<AnthropicSystemPrompt>,
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
    name: String,
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
    system: Option<AnthropicSystemPrompt>,
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
    // cache control
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControlEphemeral>,
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

// --- System prompt types (Anthropic API union: string | TextBlockParam[]) ---

/// Time-to-live for cache control breakpoints.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub enum CacheTTL {
    /// 5 minutes
    #[serde(rename = "5m")]
    FiveMinutes,
    /// 1 hour
    #[serde(rename = "1h")]
    OneHour,
}

/// Cache control configuration for ephemeral caching.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CacheControlEphemeral {
    #[serde(rename = "type")]
    pub control_type: String,
    /// Time-to-live for the cache control breakpoint. Defaults to 5m.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<CacheTTL>,
}

/// Citation referencing a character location within a document.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationCharLocationParam {
    pub cited_text: String,
    pub document_index: u64,
    pub document_title: String,
    pub end_char_index: u64,
    pub start_char_index: u64,
}

/// Citation referencing a page location within a document.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationPageLocationParam {
    pub cited_text: String,
    pub document_index: u64,
    pub document_title: String,
    pub end_page_number: u64,
    pub start_page_number: u64,
}

/// Citation referencing a content block location within a document.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationContentBlockLocationParam {
    pub cited_text: String,
    pub document_index: u64,
    pub document_title: String,
    pub end_block_index: u64,
    pub start_block_index: u64,
}

/// Citation referencing a web search result location.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationWebSearchResultLocationParam {
    pub cited_text: String,
    pub encrypted_index: String,
    pub title: String,
    pub url: String,
}

/// Citation referencing a search result location.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct CitationSearchResultLocationParam {
    pub cited_text: String,
    pub end_block_index: u64,
    pub search_result_index: u64,
    pub source: String,
    pub start_block_index: u64,
    pub title: String,
}

/// Union of all citation parameter types, discriminated by the `type` field.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum TextCitationParam {
    #[serde(rename = "char_location")]
    CharLocation(CitationCharLocationParam),
    #[serde(rename = "page_location")]
    PageLocation(CitationPageLocationParam),
    #[serde(rename = "content_block_location")]
    ContentBlockLocation(CitationContentBlockLocationParam),
    #[serde(rename = "web_search_result_location")]
    WebSearchResultLocation(CitationWebSearchResultLocationParam),
    #[serde(rename = "search_result_location")]
    SearchResultLocation(CitationSearchResultLocationParam),
}

/// A text content block used in system prompts, with optional cache control and citations.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
pub struct TextBlockParam {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<Vec<TextCitationParam>>,
}

/// Anthropic system prompt: either a plain string or an array of TextBlockParam.
///
/// Deserializes from three JSON shapes:
/// - `"string"` → `Text(String)`
/// - `["s1", "s2"]` → `Blocks` with each string wrapped as a `TextBlockParam`
/// - `[{"type":"text","text":"...","cache_control":{...}}]` → `Blocks(Vec<TextBlockParam>)`
#[derive(Debug, Clone, JsonSchema, Serialize, PartialEq)]
#[serde(untagged)]
pub enum AnthropicSystemPrompt {
    /// Plain text system prompt
    Text(String),
    /// Array of text content blocks with optional cache control and citations
    Blocks(Vec<TextBlockParam>),
}

impl<'de> Deserialize<'de> for AnthropicSystemPrompt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match value {
            Value::String(s) => Ok(AnthropicSystemPrompt::Text(s)),
            Value::Array(arr) => {
                // Try to deserialize as Vec<TextBlockParam> first (objects with "type"/"text")
                // If that fails, try as Vec<String> (plain string array from LLMParams)
                let blocks_result: Result<Vec<TextBlockParam>, _> =
                    serde_json::from_value(Value::Array(arr.clone()));
                if let Ok(blocks) = blocks_result {
                    return Ok(AnthropicSystemPrompt::Blocks(blocks));
                }

                // Try as array of plain strings
                let strings: Vec<String> = arr
                    .into_iter()
                    .map(|v| match v {
                        Value::String(s) => Ok(s),
                        other => Err(serde::de::Error::custom(format!(
                            "expected string or TextBlockParam object in system array, got {}",
                            other
                        ))),
                    })
                    .collect::<Result<_, _>>()?;
                Ok(AnthropicSystemPrompt::Blocks(
                    strings
                        .into_iter()
                        .map(|text| TextBlockParam {
                            block_type: "text".to_string(),
                            text,
                            cache_control: None,
                            citations: None,
                        })
                        .collect(),
                ))
            }
            other => Err(serde::de::Error::custom(format!(
                "expected string or array for system prompt, got {}",
                other
            ))),
        }
    }
}

/// Response from Anthropic's messages API endpoint.
#[derive(Deserialize, Debug)]
struct AnthropicCompleteResponse {
    content: Vec<AnthropicContent>,
    stop_reason: String,
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
                    content.input.clone().unwrap_or(serde_json::Value::Null)
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

    fn finish_reason(&self) -> Option<FinishReason> {
        Some(match self.stop_reason.as_ref() {
            "end_turn" => FinishReason::Stop,
            "max_tokens" => FinishReason::Length,
            "stop_sequence" => FinishReason::Stop,
            "tool_use" => FinishReason::ToolCalls,
            "refusal" | "pause_turn" => FinishReason::Other,
            _ => FinishReason::Unknown,
        })
    }
}

impl Anthropic {
    fn default_base_url() -> Url {
        Url::parse("https://api.anthropic.com/v1/").unwrap()
    }

    /// Determines the authentication type to use.
    /// Delegates to `detect_auth_type` for the actual logic.
    fn determine_auth_type(&self) -> AuthType {
        detect_auth_type(&self.api_key, self.auth_type.clone())
    }

    /// Returns true if using OAuth authentication
    fn is_oauth(&self) -> bool {
        self.determine_auth_type() == AuthType::OAuth
    }

    /// Sanitizes the system prompt for OAuth requests.
    fn sanitize_system_prompt(&self) -> Option<AnthropicSystemPrompt> {
        self.system.as_ref().map(|prompt| {
            if self.is_oauth() {
                let re = Regex::new(r"(?i)querymt").unwrap();
                match prompt {
                    AnthropicSystemPrompt::Text(s) => {
                        let result = s.replace("QueryMT", "Claude Code");
                        AnthropicSystemPrompt::Text(re.replace_all(&result, "Claude").to_string())
                    }
                    AnthropicSystemPrompt::Blocks(blocks) => AnthropicSystemPrompt::Blocks(
                        blocks
                            .iter()
                            .map(|block| {
                                let result = block.text.replace("QueryMT", "Claude Code");
                                TextBlockParam {
                                    block_type: block.block_type.clone(),
                                    text: re.replace_all(&result, "Claude").to_string(),
                                    cache_control: block.cache_control.clone(),
                                    citations: block.citations.clone(),
                                }
                            })
                            .collect(),
                    ),
                }
            } else {
                prompt.clone()
            }
        })
    }

    /// Prefixes a tool name with TOOL_PREFIX if using OAuth
    fn prefix_tool_name(&self, name: &str) -> String {
        if self.is_oauth() {
            format!("{}{}", TOOL_PREFIX, name)
        } else {
            name.to_string()
        }
    }

    /// Strips the TOOL_PREFIX from a tool name if present (used for responses)
    fn strip_tool_prefix(name: &str) -> String {
        name.strip_prefix(TOOL_PREFIX).unwrap_or(name).to_string()
    }

    /// Adds authentication headers to the request builder based on auth type
    fn add_auth_headers(&self, builder: http::request::Builder) -> http::request::Builder {
        let auth_type = self.determine_auth_type();
        let builder = match auth_type {
            AuthType::OAuth => builder
                .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
                .header(
                    "anthropic-beta",
                    "oauth-2025-04-20,interleaved-thinking-2025-05-14",
                )
                .header(USER_AGENT, "claude-cli/2.1.2 (external, cli)"),
            AuthType::ApiKey => builder.header("x-api-key", &self.api_key),
        };
        builder.header("anthropic-version", "2023-06-01")
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
            .map(|m| {
                // Build content blocks first
                let mut content = match &m.message_type {
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
                        cache_control: None,
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
                            cache_control: None,
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
                            cache_control: None,
                        }]
                    }
                    MessageType::ImageURL(url) => vec![MessageContent {
                        message_type: Some("image_url"),
                        text: None,
                        image_url: Some(ImageUrlContent { url }),
                        source: None,
                        tool_use_id: None,
                        tool_input: None,
                        tool_name: None,
                        tool_result_id: None,
                        tool_output: None,
                        cache_control: None,
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
                                cache_control: None,
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
                                tool_name: Some(self.prefix_tool_name(&c.function.name)),
                                tool_result_id: None,
                                tool_output: None,
                                cache_control: None,
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
                            cache_control: None,
                        })
                        .collect(),
                };

                // Apply cache_control to the last content block if present
                if let Some(cache_hint) = &m.cache {
                    if let Some(last) = content.last_mut() {
                        last.cache_control = Some(match cache_hint {
                            querymt::chat::CacheHint::Ephemeral { ttl_seconds } => {
                                CacheControlEphemeral {
                                    control_type: "ephemeral".to_string(),
                                    ttl: match ttl_seconds {
                                        Some(s) if *s > 300 => Some(CacheTTL::OneHour),
                                        Some(_) => Some(CacheTTL::FiveMinutes),
                                        None => None,
                                    },
                                }
                            }
                        });
                    }
                }

                AnthropicMessage {
                    role: match m.role {
                        ChatRole::User => "user",
                        ChatRole::Assistant => "assistant",
                    },
                    content,
                }
            })
            .collect();

        let maybe_tool_slice: Option<&[Tool]> = tools.or(self.tools.as_deref());
        let anthropic_tools = maybe_tool_slice.map(|slice| {
            slice
                .iter()
                .map(|tool| AnthropicTool {
                    name: self.prefix_tool_name(&tool.function.name),
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
                ("name".to_string(), self.prefix_tool_name(tool_name)),
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

        // Use sanitized system prompt for OAuth requests
        let sanitized_system = self.sanitize_system_prompt();

        let req_body = AnthropicCompleteRequest {
            messages: anthropic_messages,
            model: &self.model,
            max_tokens: Some(self.max_tokens),
            temperature: if self.reasoning.unwrap_or(false) {
                // NOTE: Ignoring temperature when reasoning is enabled. Temperature in this cases
                // should always be set to `1.0`.
                Some(1.0)
            } else {
                self.temperature
            },
            system: sanitized_system,
            stream: self.stream,
            top_p: self.top_p,
            top_k: self.top_k,
            tools: anthropic_tools,
            tool_choice: final_tool_choice,
            thinking,
        };

        let json_req = serde_json::to_vec(&req_body)?;
        let mut url = Anthropic::default_base_url().join("messages")?;

        // Add beta query parameter for OAuth requests
        if self.is_oauth() {
            url.query_pairs_mut().append_pair("beta", "true");
        }

        let builder = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json");

        let builder = self.add_auth_headers(builder);

        Ok(builder.body(json_req)?)
    }

    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        handle_http_error!(resp);

        let mut json_resp: AnthropicCompleteResponse = serde_json::from_slice(resp.body())
            .map_err(|e| LLMError::HttpError(format!("Failed to parse JSON: {}", e)))?;

        // Strip tool prefix from tool names in response (for OAuth)
        if self.is_oauth() {
            for content in &mut json_resp.content {
                if let Some(ref mut name) = content.name {
                    *name = Self::strip_tool_prefix(name);
                }
            }
        }

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
                            && block.block_type == "tool_use"
                        {
                            // Strip tool prefix from name for OAuth responses
                            let name = block.name.unwrap_or_default();
                            let name = if self.is_oauth() {
                                Self::strip_tool_prefix(&name)
                            } else {
                                name
                            };
                            chunks.push(querymt::chat::StreamChunk::ToolUseStart {
                                index,
                                id: block.id.unwrap_or_default(),
                                name,
                            });
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
                        if let Some(delta) = stream_resp.delta
                            && let Some(stop_reason) = delta.stop_reason
                        {
                            chunks.push(querymt::chat::StreamChunk::Done { stop_reason });
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
    if let Some(models) = get_env_var!("PROVIDERS_REGISTRY_DATA")
        && let Ok(registry) = serde_json::from_str::<ProvidersRegistry>(&models)
    {
        return registry.get_pricing("anthropic", model).cloned();
    }
    None
}

mod factory;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oauth_token_detection() {
        let anthropic = Anthropic {
            api_key: "sk-ant-oat01-abc123".to_string(),
            auth_type: None,
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: Some(1.0),
            timeout_seconds: None,
            system: None,
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };

        assert_eq!(anthropic.determine_auth_type(), AuthType::OAuth);
    }

    #[test]
    fn test_api_key_detection() {
        let anthropic = Anthropic {
            api_key: "sk-ant-api03-xyz789".to_string(),
            auth_type: None,
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: Some(1.0),
            timeout_seconds: None,
            system: None,
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };

        assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_explicit_auth_type_override() {
        // Even with an oat token, explicit auth_type should take precedence
        let anthropic = Anthropic {
            api_key: "sk-ant-oat01-abc123".to_string(),
            auth_type: Some(AuthType::ApiKey), // Explicitly set to API key
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: Some(1.0),
            timeout_seconds: None,
            system: None,
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };

        assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_fallback_to_api_key_for_unknown_format() {
        let anthropic = Anthropic {
            api_key: "sk-ant-unknown-format".to_string(),
            auth_type: None,
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: Some(1.0),
            timeout_seconds: None,
            system: None,
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };

        // Should default to API key and print warning
        assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_version_number_flexibility() {
        // Test with different version numbers
        let anthropic_oat99 = Anthropic {
            api_key: "sk-ant-oat99-future".to_string(),
            auth_type: None,
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: Some(1.0),
            timeout_seconds: None,
            system: None,
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };

        assert_eq!(anthropic_oat99.determine_auth_type(), AuthType::OAuth);

        let anthropic_api15 = Anthropic {
            api_key: "sk-ant-api15-future".to_string(),
            auth_type: None,
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: Some(1.0),
            timeout_seconds: None,
            system: None,
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };

        assert_eq!(anthropic_api15.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_system_prompt_deserialize_string() {
        let json = serde_json::json!({
            "api_key": "sk-ant-api03-test",
            "model": "claude-3-7-sonnet-20250219",
            "max_tokens": 100,
            "system": "You are a helpful assistant."
        });
        let anthropic: Anthropic = serde_json::from_value(json).unwrap();
        assert_eq!(
            anthropic.system,
            Some(AnthropicSystemPrompt::Text(
                "You are a helpful assistant.".to_string()
            ))
        );
    }

    #[test]
    fn test_system_prompt_deserialize_blocks() {
        let json = serde_json::json!({
            "api_key": "sk-ant-api03-test",
            "model": "claude-3-7-sonnet-20250219",
            "max_tokens": 100,
            "system": [
                {
                    "type": "text",
                    "text": "You are a helpful assistant.",
                    "cache_control": {
                        "type": "ephemeral"
                    }
                }
            ]
        });
        let anthropic: Anthropic = serde_json::from_value(json).unwrap();
        match &anthropic.system {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, "You are a helpful assistant.");
                assert_eq!(blocks[0].block_type, "text");
                assert!(blocks[0].cache_control.is_some());
                let cc = blocks[0].cache_control.as_ref().unwrap();
                assert_eq!(cc.control_type, "ephemeral");
                assert_eq!(cc.ttl, None);
            }
            other => panic!("Expected Blocks variant, got {:?}", other),
        }
    }

    #[test]
    fn test_system_prompt_deserialize_blocks_with_ttl() {
        let json = serde_json::json!({
            "api_key": "sk-ant-api03-test",
            "model": "claude-3-7-sonnet-20250219",
            "max_tokens": 100,
            "system": [
                {
                    "type": "text",
                    "text": "You are a helpful assistant.",
                    "cache_control": {
                        "type": "ephemeral",
                        "ttl": "1h"
                    }
                }
            ]
        });
        let anthropic: Anthropic = serde_json::from_value(json).unwrap();
        match &anthropic.system {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                let cc = blocks[0].cache_control.as_ref().unwrap();
                assert_eq!(cc.ttl, Some(CacheTTL::OneHour));
            }
            other => panic!("Expected Blocks variant, got {:?}", other),
        }
    }

    #[test]
    fn test_system_prompt_serialize_string() {
        let prompt = AnthropicSystemPrompt::Text("Hello".to_string());
        let json = serde_json::to_value(&prompt).unwrap();
        assert_eq!(json, serde_json::json!("Hello"));
    }

    #[test]
    fn test_system_prompt_serialize_blocks() {
        let prompt = AnthropicSystemPrompt::Blocks(vec![TextBlockParam {
            block_type: "text".to_string(),
            text: "Hello".to_string(),
            cache_control: Some(CacheControlEphemeral {
                control_type: "ephemeral".to_string(),
                ttl: Some(CacheTTL::FiveMinutes),
            }),
            citations: None,
        }]);
        let json = serde_json::to_value(&prompt).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {
                    "type": "text",
                    "text": "Hello",
                    "cache_control": {
                        "type": "ephemeral",
                        "ttl": "5m"
                    }
                }
            ])
        );
    }

    #[test]
    fn test_system_prompt_deserialize_blocks_with_citations() {
        let json = serde_json::json!({
            "api_key": "sk-ant-api03-test",
            "model": "claude-3-7-sonnet-20250219",
            "max_tokens": 100,
            "system": [
                {
                    "type": "text",
                    "text": "Context from document.",
                    "citations": [
                        {
                            "type": "char_location",
                            "cited_text": "some text",
                            "document_index": 0,
                            "document_title": "doc.pdf",
                            "start_char_index": 0,
                            "end_char_index": 9
                        },
                        {
                            "type": "page_location",
                            "cited_text": "page text",
                            "document_index": 1,
                            "document_title": "doc2.pdf",
                            "start_page_number": 1,
                            "end_page_number": 3
                        }
                    ]
                }
            ]
        });
        let anthropic: Anthropic = serde_json::from_value(json).unwrap();
        match &anthropic.system {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                let citations = blocks[0].citations.as_ref().unwrap();
                assert_eq!(citations.len(), 2);
                match &citations[0] {
                    TextCitationParam::CharLocation(c) => {
                        assert_eq!(c.cited_text, "some text");
                        assert_eq!(c.start_char_index, 0);
                        assert_eq!(c.end_char_index, 9);
                    }
                    other => panic!("Expected CharLocation, got {:?}", other),
                }
                match &citations[1] {
                    TextCitationParam::PageLocation(p) => {
                        assert_eq!(p.cited_text, "page text");
                        assert_eq!(p.start_page_number, 1);
                        assert_eq!(p.end_page_number, 3);
                    }
                    other => panic!("Expected PageLocation, got {:?}", other),
                }
            }
            other => panic!("Expected Blocks variant, got {:?}", other),
        }
    }

    #[test]
    fn test_sanitize_system_prompt_oauth_text() {
        let anthropic = Anthropic {
            api_key: "sk-ant-oat01-abc123".to_string(),
            auth_type: None,
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: None,
            timeout_seconds: None,
            system: Some(AnthropicSystemPrompt::Text(
                "You are QueryMT assistant.".to_string(),
            )),
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };
        let sanitized = anthropic.sanitize_system_prompt();
        assert_eq!(
            sanitized,
            Some(AnthropicSystemPrompt::Text(
                "You are Claude assistant.".to_string()
            ))
        );
    }

    #[test]
    fn test_sanitize_system_prompt_oauth_blocks() {
        let anthropic = Anthropic {
            api_key: "sk-ant-oat01-abc123".to_string(),
            auth_type: None,
            model: "claude-3-7-sonnet-20250219".to_string(),
            max_tokens: 100,
            temperature: None,
            timeout_seconds: None,
            system: Some(AnthropicSystemPrompt::Blocks(vec![TextBlockParam {
                block_type: "text".to_string(),
                text: "You are QueryMT assistant.".to_string(),
                cache_control: Some(CacheControlEphemeral {
                    control_type: "ephemeral".to_string(),
                    ttl: None,
                }),
                citations: None,
            }])),
            stream: None,
            top_p: None,
            top_k: None,
            tools: None,
            tool_choice: None,
            reasoning: None,
            thinking_budget_tokens: None,
        };
        let sanitized = anthropic.sanitize_system_prompt();
        match sanitized {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks[0].text, "You are Claude assistant.");
                // cache_control should be preserved
                assert!(blocks[0].cache_control.is_some());
            }
            other => panic!("Expected Blocks variant, got {:?}", other),
        }
    }

    #[test]
    fn test_usage_deserialization_with_cache() {
        // Real fixture from Anthropic API response with cache creation and read tokens
        let json = r#"{
            "input_tokens": 12,
            "cache_creation_input_tokens": 1495,
            "cache_read_input_tokens": 0,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 1495,
                "ephemeral_1h_input_tokens": 0
            },
            "output_tokens": 1024,
            "service_tier": "standard"
        }"#;

        let usage: Usage = serde_json::from_str(json).unwrap();

        // Verify standard token counts
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 1024);

        // Verify cache-related fields (using Anthropic aliases)
        assert_eq!(usage.cache_write, 1495); // from cache_creation_input_tokens
        assert_eq!(usage.cache_read, 0); // from cache_read_input_tokens

        // Verify default values for fields not in the JSON
        assert_eq!(usage.reasoning_tokens, 0);

        // Note: Extra fields like "cache_creation" and "service_tier" are
        // silently ignored during deserialization as they're not part of Usage struct
    }
}
