//! Anthropic API client implementation for chat and completion functionality.
//!
//! This module provides integration with Anthropic's Claude models through their API.

use std::collections::HashMap;

mod model;
use model::{ClaudeModel, ThinkingClass, parse_claude_model, resolve_thinking_class};

/// Tool name prefix used for OAuth requests to avoid conflicts with server-side tools
const TOOL_PREFIX: &str = "mcp_";

/// Smallest thinking budget the API accepts.
const MIN_THINKING_BUDGET: u32 = 1_024;

/// OAuth system prompt
const OAUTH_SYSTEM_PROMPT: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const CLAUDE_CODE_VERSION: &str = "2.1.284";

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT},
};
use querymt::{
    HTTPLLMProvider, Usage,
    auth::ApiKeyResolver,
    chat::{
        ChatFunctionCallItem, ChatInputPart, ChatMessage, ChatMessageItem, ChatMessagePart,
        ChatOutput, ChatOutputItem, ChatOutputStatus, ChatReasoningItem, ChatReasoningPart,
        ChatRole, FinishReason, GenerationOptionSupport, GenerationOptions, MediaKind, MediaPart,
        MediaSource, ReasoningEffort, Tool, ToolChoice, ToolResultPart,
        http::{ChatStreamParser, HTTPChatProvider},
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::{
        LLMError, ProviderErrorKind, ProviderFailure, classify_status_only, parse_retry_after,
        parse_retry_after_from_message,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
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
    /// Reported OAuth client version (major.minor.patch), shared by headers and billing metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub timeout_seconds: Option<u64>,
    #[schemars(schema_with = "schema_for_anthropic_system")]
    pub system: Option<AnthropicSystemPrompt>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_budget_tokens: Option<u32>,
    /// Optional override for how thinking is requested.
    ///
    /// By default the thinking mode is derived from the model ID. `true`
    /// forces opt-in adaptive thinking (`thinking.type = "adaptive"`), which is
    /// the safe choice for proxy or unrecognised model IDs that front an
    /// adaptive model. `false` forces budget thinking
    /// (`thinking.type = "enabled"` with `budget_tokens`), which the API
    /// rejects on Opus 4.7+ and every Claude 5 or later model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    /// Optional resolver for dynamic credential refresh (e.g., OAuth tokens).
    #[serde(skip)]
    #[schemars(skip)]
    pub key_resolver: Option<Arc<dyn ApiKeyResolver>>,
}

/// Per-block accumulator used while streaming tool-use content.
///
/// Anthropic's SSE protocol emits tool arguments as a sequence of
/// `input_json_delta` events followed by a `content_block_stop`.  We
/// accumulate the partial JSON here and emit a `ToolUseComplete` chunk
/// when the block closes.
#[derive(Debug, Default)]
struct AnthropicToolUseState {
    id: String,
    name: String,
    arguments_buffer: String,
}

/// Per-block accumulator for Anthropic thinking signature deltas.
#[derive(Debug, Default)]
struct AnthropicThinkingState {
    signature: String,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<u32>,
}

/// Output configuration (thinking depth) for adaptive models.
#[derive(Serialize, Debug)]
struct OutputConfig {
    effort: &'static str,
}

/// Request payload for Anthropic's messages API endpoint.
#[derive(Serialize, Debug)]
struct AnthropicCompleteRequest<'a> {
    messages: Vec<AnthropicMessage>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<OutputConfig>,
}

/// Individual message in an Anthropic chat conversation.
#[derive(Serialize, Debug)]
struct AnthropicMessage {
    role: &'static str,
    content: Vec<MessageContent>,
}

/// A content block sent to the Anthropic API.
///
/// Uses `#[serde(untagged)]` so each variant controls its own `type` field.
/// This avoids needing a single flat struct with many `Option` fields.
#[derive(Serialize, Debug)]
#[serde(untagged)]
enum MessageContent {
    Text {
        #[serde(rename = "type")]
        content_type: &'static str, // "text"
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Image {
        #[serde(rename = "type")]
        content_type: &'static str, // "image"
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    ImageUrl {
        #[serde(rename = "type")]
        content_type: &'static str, // "image_url"
        image_url: ImageUrlContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Document {
        #[serde(rename = "type")]
        content_type: &'static str, // "document"
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Thinking {
        #[serde(rename = "type")]
        content_type: &'static str, // "thinking"
        thinking: String,
        signature: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    RedactedThinking {
        #[serde(rename = "type")]
        content_type: &'static str, // "redacted_thinking"
        data: String,
    },
    ToolUse {
        #[serde(rename = "type")]
        content_type: &'static str, // "tool_use"
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    ToolResult {
        #[serde(rename = "type")]
        content_type: &'static str, // "tool_result"
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        content: Vec<ToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
}

/// Content block inside a tool_result (Anthropic supports text + image).
#[derive(Serialize, Debug)]
#[serde(untagged)]
enum ToolResultContent {
    Text {
        #[serde(rename = "type")]
        content_type: &'static str,
        text: String,
    },
    Image {
        #[serde(rename = "type")]
        content_type: &'static str,
        source: ImageSource,
    },
}

impl MessageContent {
    fn set_cache_control(&mut self, cc: CacheControlEphemeral) {
        match self {
            MessageContent::Text { cache_control, .. }
            | MessageContent::Image { cache_control, .. }
            | MessageContent::ImageUrl { cache_control, .. }
            | MessageContent::Document { cache_control, .. }
            | MessageContent::Thinking { cache_control, .. }
            | MessageContent::ToolUse { cache_control, .. }
            | MessageContent::ToolResult { cache_control, .. } => {
                *cache_control = Some(cc);
            }
            // `redacted_thinking` blocks accept no cache_control.
            MessageContent::RedactedThinking { .. } => {}
        }
    }
}

#[derive(Serialize, Debug)]
struct ImageUrlContent {
    url: String,
}

#[derive(Serialize, Debug)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: &'static str,
    media_type: String,
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

fn schema_for_anthropic_system(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let portable = generator.subschema_for::<querymt::params::SystemPrompt>();
    let native = generator.subschema_for::<TextBlockParam>();
    serde_json::from_value(serde_json::json!({"anyOf": [
        {"type": "null"}, portable, native,
        {"type": "array", "items": {"anyOf": [portable, native]}}
    ]}))
    .expect("valid Anthropic instruction schema")
}

fn cache_control_from_hint(hint: &querymt::chat::CacheHint) -> CacheControlEphemeral {
    let querymt::chat::CacheHint::Ephemeral { ttl_seconds } = hint;
    CacheControlEphemeral {
        control_type: "ephemeral".into(),
        ttl: match ttl_seconds {
            Some(seconds) if *seconds > 300 => Some(CacheTTL::OneHour),
            Some(_) => Some(CacheTTL::FiveMinutes),
            None => None,
        },
    }
}

impl<'de> Deserialize<'de> for AnthropicSystemPrompt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let value = if value.is_object() {
            Value::Array(vec![value])
        } else {
            value
        };
        match value {
            Value::String(s) => Ok(AnthropicSystemPrompt::Text(s)),
            Value::Array(arr) => {
                let mut blocks = Vec::with_capacity(arr.len());
                for value in arr {
                    if value.get("type").is_some() {
                        let hint = value
                            .get("cache")
                            .filter(|v| !v.is_null())
                            .map(|hint| {
                                serde_json::from_value::<querymt::chat::CacheHint>(hint.clone())
                            })
                            .transpose()
                            .map_err(serde::de::Error::custom)?;
                        let mut block: TextBlockParam =
                            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                        if block.cache_control.is_none() {
                            block.cache_control = hint.as_ref().map(cache_control_from_hint);
                        }
                        blocks.push(block);
                        continue;
                    }
                    let part: querymt::params::SystemPrompt =
                        serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                    let cache_control = part.cache().map(cache_control_from_hint);
                    blocks.push(TextBlockParam {
                        block_type: "text".into(),
                        text: part.text().to_owned(),
                        cache_control,
                        citations: None,
                    });
                }
                Ok(AnthropicSystemPrompt::Blocks(blocks))
            }
            other => Err(serde::de::Error::custom(format!(
                "expected string or array for system prompt, got {}",
                other
            ))),
        }
    }
}

/// Thinking-token breakdown nested under `usage.output_tokens_details`.
#[derive(Deserialize, Debug, Clone, Default)]
struct AnthropicOutputTokensDetails {
    #[serde(default)]
    thinking_tokens: Option<u32>,
}

/// Provider-local usage object.
///
/// Anthropic nests the thinking-token count under
/// `output_tokens_details.thinking_tokens`, which the shared `querymt::Usage`
/// cannot deserialize directly. `thinking_tokens` are already included in
/// `output_tokens`; the `Usage` conversion subtracts them so the shared
/// output and reasoning categories stay exclusive.
#[derive(Deserialize, Debug, Clone, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,
    #[serde(default)]
    cache_read_input_tokens: u32,
    #[serde(default)]
    output_tokens_details: Option<AnthropicOutputTokensDetails>,
}

impl From<AnthropicUsage> for Usage {
    fn from(usage: AnthropicUsage) -> Self {
        // Anthropic's `output_tokens` includes thinking tokens, while QueryMT's
        // output and reasoning categories are exclusive (shared cost code adds
        // them back together), so subtract here to avoid double counting.
        let reasoning = usage
            .output_tokens_details
            .and_then(|d| d.thinking_tokens)
            .unwrap_or(0);
        Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens.saturating_sub(reasoning),
            reasoning_tokens: reasoning,
            cache_read: usage.cache_read_input_tokens,
            cache_write: usage.cache_creation_input_tokens,
        }
    }
}

/// Details accompanying a non-`end_turn` stop reason (e.g. refusal category).
#[derive(Deserialize, Debug, Clone, Default)]
struct AnthropicStopDetails {
    #[serde(default)]
    category: Option<String>,
}

/// Response from Anthropic's messages API endpoint.
#[derive(Deserialize, Debug)]
struct AnthropicCompleteResponse {
    content: Vec<AnthropicContent>,
    stop_reason: String,
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    stop_details: Option<AnthropicStopDetails>,
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
    /// Usage information (present on message_delta events; only output_tokens is populated)
    usage: Option<AnthropicUsage>,
    /// Nested message object (present on message_start events)
    message: Option<AnthropicStreamMessage>,
    /// Nested error object (present on `type: "error"` SSE events)
    error: Option<AnthropicApiErrorBody>,
}

/// Anthropic error envelope body (`{"type":"error","error":{...}}` or nested in SSE).
#[derive(Deserialize, Debug)]
struct AnthropicApiErrorBody {
    #[serde(rename = "type")]
    error_type: Option<String>,
    message: Option<String>,
}

/// Nested message object within an Anthropic `message_start` SSE event.
#[derive(Deserialize, Debug)]
struct AnthropicStreamMessage {
    /// Usage information present in the initial message_start event.
    usage: Option<AnthropicUsage>,
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
    /// Opaque payload (for redacted_thinking blocks)
    data: Option<String>,
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
    /// Thinking signature (for signature_delta)
    signature: Option<String>,
    /// Stop reason (for message_delta)
    stop_reason: Option<String>,
    /// Stop details (for message_delta), e.g. refusal category
    #[serde(default)]
    stop_details: Option<AnthropicStopDetails>,
}

/// Content block within an Anthropic API response.
#[derive(Serialize, Deserialize, Debug)]
struct AnthropicContent {
    text: Option<String>,
    #[serde(rename = "type")]
    content_type: Option<String>,
    thinking: Option<String>,
    /// Thinking signature (for thinking blocks)
    #[serde(default)]
    signature: Option<String>,
    /// Opaque payload (for redacted_thinking blocks)
    #[serde(default)]
    data: Option<String>,
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

impl From<AnthropicCompleteResponse> for ChatOutput {
    fn from(response: AnthropicCompleteResponse) -> Self {
        let mut items: Vec<ChatOutputItem> = Vec::new();

        // Build items in content order so thinking blocks keep their own
        // signature and position relative to text and tool calls.
        for block in &response.content {
            match block.content_type.as_deref() {
                Some("thinking") => {
                    let text = block.thinking.clone().unwrap_or_default();
                    let signature = block.signature.clone().filter(|s| !s.is_empty());
                    if text.is_empty() && signature.is_none() {
                        continue;
                    }
                    items.push(ChatOutputItem::Reasoning(ChatReasoningItem {
                        id: None,
                        summary: Vec::new(),
                        content: if text.is_empty() {
                            Vec::new()
                        } else {
                            vec![ChatReasoningPart::text(text)]
                        },
                        encrypted_content: None,
                        signature,
                        status: None,
                        extensions: Default::default(),
                    }));
                }
                Some("redacted_thinking") => {
                    if let Some(data) = block.data.clone() {
                        items.push(redacted_reasoning_item(data));
                    }
                }
                Some("tool_use") => {
                    items.push(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                        item_id: None,
                        call_id: block.id.clone().unwrap_or_default(),
                        name: block.name.clone().unwrap_or_default(),
                        arguments: serde_json::to_string(
                            &block.input.clone().unwrap_or(serde_json::Value::Null),
                        )
                        .unwrap_or_default(),
                        status: None,
                        extensions: Default::default(),
                    }));
                }
                Some("text") | None => {
                    let Some(text) = block.text.clone().filter(|t| !t.is_empty()) else {
                        continue;
                    };
                    // Consecutive text blocks form one message.
                    if let Some(ChatOutputItem::Message(message)) = items.last_mut()
                        && let Some(ChatMessagePart::Text { text: existing, .. }) =
                            message.parts.last_mut()
                    {
                        existing.push('\n');
                        existing.push_str(&text);
                        continue;
                    }
                    items.push(ChatOutputItem::Message(ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![ChatMessagePart::Text {
                            text,
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    }));
                }
                Some(_) => {}
            }
        }

        let finish_reason = Anthropic::map_stop_reason_with_details(
            &response.stop_reason,
            response.stop_details.as_ref(),
        );

        ChatOutput {
            items,
            status: Some(ChatOutputStatus::Completed),
            usage: response.usage.map(Usage::from),
            finish_reason: Some(finish_reason),
            ..ChatOutput::default()
        }
    }
}

/// Build the reasoning item that carries a `redacted_thinking` payload.
fn redacted_reasoning_item(data: String) -> ChatOutputItem {
    let mut extensions = querymt::chat::Extensions::new();
    extensions.insert(
        querymt::chat::ANTHROPIC_BLOCK_EXTENSION.to_string(),
        Value::String(querymt::chat::ANTHROPIC_REDACTED_THINKING.to_string()),
    );
    ChatOutputItem::Reasoning(ChatReasoningItem {
        id: None,
        summary: Vec::new(),
        content: Vec::new(),
        encrypted_content: Some(data),
        signature: None,
        status: None,
        extensions,
    })
}

fn oauth_text_block(text: String) -> TextBlockParam {
    TextBlockParam {
        block_type: "text".to_string(),
        text,
        cache_control: None,
        citations: None,
    }
}

/// Identity lines injected by QueryMT application prompts.
///
/// OAuth requests already carry Anthropic's Claude identity via
/// `OAUTH_SYSTEM_PROMPT`, so application-level identity declarations would
/// compete with it and are pruned. Everything else — operational rules,
/// `<env>`/`<directories>` metadata, mentions of QueryMT in instructions —
/// is preserved verbatim.
const IDENTITY_LINE_PREFIXES: [&str; 2] =
    ["You are QueryMT, a coding agent", "You are powered by "];

fn sanitize_oauth_text(text: &str) -> String {
    // Drop only whole lines that declare an application identity.
    let kept: Vec<&str> = text
        .split('\n')
        .filter(|line| {
            !IDENTITY_LINE_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
        .collect();

    // Collapse paragraphs left empty by the pruning above.
    let mut paragraphs = Vec::new();
    let mut current = Vec::new();
    for line in kept {
        if line.is_empty() {
            if !current.is_empty() {
                paragraphs.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        paragraphs.push(current.join("\n"));
    }
    paragraphs.join("\n\n").trim().to_string()
}

fn oauth_billing_header(text: &str, version: &str) -> String {
    let cch: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // JavaScript indexes UTF-16 code units, not UTF-8 bytes or Unicode scalars.
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let sampled: Vec<u16> = [4, 7, 20]
        .iter()
        .map(|&i| utf16.get(i).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let suffix: String = Sha256::digest(
        format!(
            "59cf53e54c78{}{}",
            String::from_utf16_lossy(&sampled),
            version
        )
        .as_bytes(),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
    format!(
        "x-anthropic-billing-header: cc_version={version}.{}; cc_entrypoint=sdk-cli; cch={};",
        &suffix[..3],
        &cch[..5]
    )
}

fn validate_client_version(version: &str) -> Result<(), LLMError> {
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(LLMError::InvalidRequest(
            "claude_code_version must be major.minor.patch".to_string(),
        ));
    }
    Ok(())
}

impl Anthropic {
    fn oauth_version(&self) -> &str {
        self.claude_code_version
            .as_deref()
            .map(str::trim)
            .unwrap_or(CLAUDE_CODE_VERSION)
    }

    fn validate_oauth_version(&self) -> Result<(), LLMError> {
        validate_client_version(self.oauth_version())
    }

    /// Map a raw Anthropic `stop_reason` string to a typed `FinishReason`.
    ///
    /// This is the single mapping used by both the streaming and
    /// non-streaming paths.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn map_stop_reason(stop_reason: &str) -> FinishReason {
        Self::map_stop_reason_with_details(stop_reason, None)
    }

    /// Like [`map_stop_reason`], additionally logging the raw reason and, for
    /// refusals, the reported `stop_details.category`.
    pub(crate) fn map_stop_reason_with_details(
        stop_reason: &str,
        details: Option<&AnthropicStopDetails>,
    ) -> FinishReason {
        let finish_reason = match stop_reason {
            "end_turn" | "stop_sequence" => FinishReason::Stop,
            "tool_use" => FinishReason::ToolCalls,
            "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
            "refusal" => FinishReason::ContentFilter,
            "pause_turn" => FinishReason::Other,
            _ => FinishReason::Unknown,
        };
        log::debug!(target: "qmt_anthropic", "stop_reason={stop_reason:?} -> {finish_reason:?}");
        if stop_reason == "refusal"
            && let Some(category) = details.and_then(|d| d.category.as_deref())
        {
            log::info!(target: "qmt_anthropic", "refusal stop_details.category={category}");
        }
        finish_reason
    }

    fn default_base_url() -> Url {
        Url::parse("https://api.anthropic.com/v1/").unwrap()
    }

    /// Returns the current API key, using the resolver if available.
    fn resolved_key(&self) -> String {
        if let Some(ref resolver) = self.key_resolver {
            resolver.current()
        } else {
            self.api_key.clone()
        }
    }

    /// Determines the authentication type to use.
    /// Delegates to `detect_auth_type` for the actual logic.
    fn determine_auth_type(&self) -> AuthType {
        let key = self.resolved_key();
        detect_auth_type(&key, self.auth_type.clone())
    }

    /// Returns true if using OAuth authentication
    fn is_oauth(&self) -> bool {
        self.determine_auth_type() == AuthType::OAuth
    }

    /// Sanitizes the system prompt for OAuth requests.
    ///
    /// For OAuth: prunes application identity lines (see
    /// `IDENTITY_LINE_PREFIXES`), drops blocks left empty by that pruning,
    /// and prepends `OAUTH_SYSTEM_PROMPT` as the first block, converting a
    /// plain `Text` prompt to `Blocks` in the process. When no system prompt
    /// is configured and OAuth is active, returns the `OAUTH_SYSTEM_PROMPT`
    /// const wrapped in a single-element `Blocks`.
    ///
    /// For non-OAuth: returns the configured system prompt unchanged.
    fn sanitize_system_prompt(&self) -> Option<AnthropicSystemPrompt> {
        if !self.is_oauth() {
            return self.system.clone();
        }
        let mut blocks = match &self.system {
            None => Vec::new(),
            Some(AnthropicSystemPrompt::Text(text)) => {
                vec![oauth_text_block(sanitize_oauth_text(text))]
            }
            Some(AnthropicSystemPrompt::Blocks(blocks)) => blocks
                .iter()
                .cloned()
                .map(|mut block| {
                    block.text = sanitize_oauth_text(&block.text);
                    block
                })
                .collect(),
        };
        // Anthropic rejects empty text blocks; identity-only prompts prune to
        // nothing here and only the injected identity block remains.
        blocks.retain(|block| !block.text.is_empty());
        // Keep the identity separate from cached application instructions.
        if blocks
            .first()
            .is_none_or(|block| block.text != OAUTH_SYSTEM_PROMPT)
        {
            blocks.insert(0, oauth_text_block(OAUTH_SYSTEM_PROMPT.to_string()));
        }
        Some(AnthropicSystemPrompt::Blocks(blocks))
    }

    /// Prefixes a tool name with TOOL_PREFIX if using OAuth
    fn prefix_tool_name(&self, name: &str) -> String {
        if self.is_oauth() {
            let mut chars = name.chars();
            format!(
                "{}{}{}",
                TOOL_PREFIX,
                chars
                    .next()
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_default(),
                chars.as_str()
            )
        } else {
            name.to_string()
        }
    }

    /// Copy of this config with per-request overrides applied.
    ///
    /// Every downstream decision (budget fitting against `max_tokens`, adaptive
    /// effort, sampling restrictions) reads from the config, so applying the
    /// overrides up front keeps one code path for configured and per-request
    /// values. An effort override also discards a configured explicit
    /// `reasoning_budget_tokens`; otherwise that budget would silently win over
    /// the requested effort.
    fn with_generation_options(&self, options: &GenerationOptions) -> Self {
        let mut cfg = self.clone();
        if let Some(max_tokens) = options.max_tokens {
            cfg.max_tokens = max_tokens;
        }
        if let Some(effort) = options.reasoning_effort {
            cfg.reasoning_effort = Some(effort);
            cfg.reasoning_budget_tokens = None;
        }
        cfg
    }

    /// Parsed Claude model, or `None` for IDs that aren't recognisable.
    fn parsed_model(&self) -> Option<ClaudeModel> {
        parse_claude_model(&self.model)
    }

    /// Thinking class for this model, honouring `adaptive_thinking`.
    fn thinking_class(&self) -> ThinkingClass {
        resolve_thinking_class(self.parsed_model().as_ref(), self.adaptive_thinking)
    }

    /// Maps generic reasoning effort to Anthropic default budget tokens.
    fn effort_budget_tokens(effort: ReasoningEffort) -> u32 {
        match effort {
            ReasoningEffort::Low => 1_024,
            ReasoningEffort::Medium => 8_000,
            ReasoningEffort::High => 16_000,
            ReasoningEffort::Max => 31_999,
        }
    }

    /// Fit a thinking budget to `max_tokens`: at most half, at least 1,024.
    /// Returns `None` when the cap can't fit the minimum budget.
    fn fit_budget(budget: u32, max_tokens: u32) -> Option<u32> {
        if max_tokens < 2 * MIN_THINKING_BUDGET {
            return None;
        }
        Some(budget.clamp(MIN_THINKING_BUDGET, max_tokens / 2))
    }

    /// Wire name of an effort level for `output_config.effort`.
    fn effort_wire_name(effort: ReasoningEffort) -> &'static str {
        match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }

    /// Decide `thinking` and `output_config` from the model class and effort.
    fn thinking_for_request(&self) -> (Option<ThinkingConfig>, Option<OutputConfig>) {
        let adaptive = || ThinkingConfig {
            thinking_type: "adaptive".to_string(),
            budget_tokens: None,
        };
        match (self.thinking_class(), self.reasoning_effort) {
            (ThinkingClass::DefaultOnAdaptive, None) | (ThinkingClass::Budget, None) => {
                (None, None)
            }
            (ThinkingClass::OptInAdaptive, None) => (Some(adaptive()), None),
            (ThinkingClass::DefaultOnAdaptive | ThinkingClass::OptInAdaptive, Some(effort)) => (
                Some(adaptive()),
                Some(OutputConfig {
                    effort: Self::effort_wire_name(effort),
                }),
            ),
            (ThinkingClass::Budget, Some(effort)) => {
                let budget = self
                    .reasoning_budget_tokens
                    .unwrap_or_else(|| Self::effort_budget_tokens(effort));
                match Self::fit_budget(budget, self.max_tokens) {
                    Some(budget_tokens) => (
                        Some(ThinkingConfig {
                            thinking_type: "enabled".to_string(),
                            budget_tokens: Some(budget_tokens),
                        }),
                        None,
                    ),
                    None => {
                        log::debug!(target: "qmt_anthropic", "max_tokens={} too small for thinking; omitting thinking", self.max_tokens);
                        (None, None)
                    }
                }
            }
        }
    }

    /// Sampling parameters valid for this model and thinking choice.
    fn sampling_for(&self, thinking_sent: bool) -> (Option<f32>, Option<f32>, Option<u32>) {
        let reject_all = self
            .parsed_model()
            .is_some_and(|m| m.rejects_sampling_params());
        let log_drop = |name: &str, why: &str| {
            log::debug!(target: "qmt_anthropic", "dropping {name} for {}: {why}", self.model);
        };
        if reject_all {
            if self.temperature.is_some() {
                log_drop("temperature", "model rejects sampling parameters");
            }
            if self.top_p.is_some() {
                log_drop("top_p", "model rejects sampling parameters");
            }
            if self.top_k.is_some() {
                log_drop("top_k", "model rejects sampling parameters");
            }
            return (None, None, None);
        }
        if thinking_sent {
            if self.temperature.is_some() {
                log_drop("temperature", "incompatible with thinking");
            }
            if self.top_k.is_some() {
                log_drop("top_k", "incompatible with thinking");
            }
            let top_p = self.top_p.filter(|p| (0.95..=1.0).contains(p));
            if self.top_p.is_some() && top_p.is_none() {
                log_drop("top_p", "outside 0.95-1 with thinking");
            }
            return (None, top_p, None);
        }
        (self.temperature, self.top_p, self.top_k)
    }

    /// Wire-to-original names for the effective request tools, never shared
    /// between requests or streams.
    fn declared_tool_names(
        &self,
        tools: Option<&[Tool]>,
    ) -> Result<HashMap<String, String>, LLMError> {
        let mut names = HashMap::new();
        for tool in tools.or(self.tools.as_deref()).unwrap_or_default() {
            let original = &tool.function.name;
            let wire = self.prefix_tool_name(original);
            if let Some(previous) = names.insert(wire.clone(), original.clone()) {
                return Err(LLMError::InvalidRequest(format!(
                    "Tool names {previous:?} and {original:?} both encode as {wire:?}",
                )));
            }
        }
        Ok(names)
    }

    fn tool_names_from_context(&self, context: Value) -> Result<HashMap<String, String>, LLMError> {
        if context.is_null() {
            self.declared_tool_names(None)
        } else {
            Ok(serde_json::from_value(context)?)
        }
    }

    /// Reverses [`prefix_tool_name`] for tool names echoed back by the API.
    ///
    /// Names that never carried [`TOOL_PREFIX`] were never transformed and are
    /// returned unchanged so their capitalization is preserved exactly. Prefixed
    /// names are restored through the request's wire-to-original mapping,
    /// falling back to legacy lowercasing only for unknown prefixed names.
    fn strip_tool_prefix(name: &str, declared: Option<&HashMap<String, String>>) -> String {
        let Some(stripped) = name.strip_prefix(TOOL_PREFIX) else {
            return name.to_string();
        };
        if let Some(original) = declared.and_then(|names| names.get(name)) {
            return original.clone();
        }
        if stripped == "StructuredOutput" {
            return stripped.to_string();
        }
        let mut chars = stripped.chars();
        format!(
            "{}{}",
            chars
                .next()
                .map(|c| c.to_lowercase().to_string())
                .unwrap_or_default(),
            chars.as_str()
        )
    }

    /// Adds authentication headers to the request builder based on auth type
    fn add_auth_headers(&self, builder: http::request::Builder) -> http::request::Builder {
        let key = self.resolved_key();
        let auth_type = self.determine_auth_type();
        let builder = match auth_type {
            AuthType::OAuth => builder
                .header(AUTHORIZATION, format!("Bearer {}", key))
                .header(
                    "anthropic-beta",
                    "oauth-2025-04-20,interleaved-thinking-2025-05-14",
                )
                .header(
                    USER_AGENT,
                    format!("claude-cli/{} (external, cli)", self.oauth_version()),
                ),
            AuthType::ApiKey => builder.header("x-api-key", &key),
        };
        builder.header("anthropic-version", "2023-06-01")
    }
}

/// Convert a canonical attachment into an Anthropic message content block.
///
/// Unsupported sources (provider file references) return `None` rather than
/// fabricating content.
fn attachment_to_message_content(media: &MediaPart) -> Option<MessageContent> {
    match (&media.kind, media.source()) {
        (MediaKind::Image, MediaSource::Inline { data }) => {
            let media_type = media.media_type()?.to_string();
            Some(MessageContent::Image {
                content_type: "image",
                source: ImageSource {
                    source_type: "base64",
                    media_type,
                    data: BASE64.encode(data),
                },
                cache_control: None,
            })
        }
        (MediaKind::Image, MediaSource::DataUrl { url } | MediaSource::Url { url }) => {
            Some(MessageContent::ImageUrl {
                content_type: "image_url",
                image_url: ImageUrlContent { url: url.clone() },
                cache_control: None,
            })
        }
        (MediaKind::Document, MediaSource::Inline { data }) => {
            let media_type = media
                .media_type()
                .map(ToString::to_string)
                .unwrap_or_else(|| "application/pdf".to_string());
            Some(MessageContent::Document {
                content_type: "document",
                source: ImageSource {
                    source_type: "base64",
                    media_type,
                    data: BASE64.encode(data),
                },
                cache_control: None,
            })
        }
        _ => None,
    }
}

/// Convert one canonical tool-result part into an Anthropic result content block.
fn tool_result_content(part: &ToolResultPart) -> Option<ToolResultContent> {
    match part {
        ToolResultPart::Text { text } => Some(ToolResultContent::Text {
            content_type: "text",
            text: text.clone(),
        }),
        ToolResultPart::Attachment(media) => match media.source() {
            MediaSource::Inline { data } if media.kind == MediaKind::Image => {
                Some(ToolResultContent::Image {
                    content_type: "image",
                    source: ImageSource {
                        source_type: "base64",
                        media_type: media.media_type()?.to_string(),
                        data: BASE64.encode(data),
                    },
                })
            }
            _ => None,
        },
    }
}

impl HTTPChatProvider for Anthropic {
    fn prepare_chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
        stream: bool,
    ) -> Result<querymt::chat::http::PreparedChatRequest, LLMError> {
        let names = self.declared_tool_names(tools)?;
        let request = if stream {
            self.chat_stream_request(messages, tools)?
        } else {
            self.chat_request(messages, tools)?
        };
        Ok(querymt::chat::http::PreparedChatRequest {
            request,
            context: serde_json::to_value(names)?,
        })
    }

    fn supported_generation_options(&self) -> GenerationOptionSupport {
        GenerationOptionSupport::ALL
    }

    fn prepare_chat_request_with_options(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
        stream: bool,
        options: &GenerationOptions,
    ) -> Result<querymt::chat::http::PreparedChatRequest, LLMError> {
        if options.is_empty() {
            return self.prepare_chat_request(messages, tools, stream);
        }
        self.with_generation_options(options)
            .prepare_chat_request(messages, tools, stream)
    }

    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        if self.resolved_key().is_empty() {
            return Err(LLMError::AuthError("Missing Anthropic API key".to_string()));
        }
        self.declared_tool_names(tools)?;

        let anthropic_messages: Vec<AnthropicMessage> = messages
            .iter()
            .map(|m| {
                let mut content: Vec<MessageContent> = Vec::new();

                // Replay generated reasoning and function calls from structured
                // output; these are output-only semantics.
                if let Some(output) = m.output() {
                    for item in &output.items {
                        match item {
                            ChatOutputItem::Reasoning(reasoning) => {
                                let is_redacted = reasoning
                                    .extensions
                                    .get(querymt::chat::ANTHROPIC_BLOCK_EXTENSION)
                                    .and_then(Value::as_str)
                                    == Some(querymt::chat::ANTHROPIC_REDACTED_THINKING);
                                if is_redacted {
                                    // Opaque payload, replayed unchanged.
                                    if let Some(data) = &reasoning.encrypted_content {
                                        content.push(MessageContent::RedactedThinking {
                                            content_type: "redacted_thinking",
                                            data: data.clone(),
                                        });
                                    }
                                } else if let Some(signature) = &reasoning.signature {
                                    // Anthropic requires signed thinking blocks when
                                    // replaying assistant history, even when the text
                                    // is empty (display "omitted"). Unsigned ones are
                                    // skipped.
                                    let text = reasoning
                                        .summary
                                        .iter()
                                        .chain(&reasoning.content)
                                        .map(|part| part.text.as_str())
                                        .collect::<Vec<_>>()
                                        .join("\n\n");
                                    content.push(MessageContent::Thinking {
                                        content_type: "thinking",
                                        thinking: text,
                                        signature: signature.clone(),
                                        cache_control: None,
                                    });
                                }
                            }
                            ChatOutputItem::FunctionCall(call) => {
                                // Anthropic requires `input` to be a JSON object.
                                let input = call.parse_arguments().unwrap_or(Value::Null);
                                let input = if input.is_object() {
                                    input
                                } else {
                                    Value::Object(Default::default())
                                };
                                content.push(MessageContent::ToolUse {
                                    content_type: "tool_use",
                                    id: call.call_id.clone(),
                                    name: self.prefix_tool_name(&call.name),
                                    input,
                                    cache_control: None,
                                });
                            }
                            _ => {}
                        }
                    }
                }

                for part in m.portable_input_parts() {
                    match part {
                        ChatInputPart::Text { text } => {
                            // Avoid Anthropic API error: "text content blocks must be non-empty"
                            if !text.is_empty() {
                                content.push(MessageContent::Text {
                                    content_type: "text",
                                    text,
                                    cache_control: None,
                                });
                            }
                        }
                        ChatInputPart::Attachment(media) => {
                            if let Some(block) = attachment_to_message_content(&media) {
                                content.push(block);
                            }
                        }
                        ChatInputPart::ToolResult(result) => {
                            // Anthropic supports multi-content tool results natively:
                            // { type: "tool_result", tool_use_id, content: [text, image, ...] }
                            let tool_content: Vec<ToolResultContent> = result
                                .parts
                                .iter()
                                .filter_map(tool_result_content)
                                .collect();
                            content.push(MessageContent::ToolResult {
                                content_type: "tool_result",
                                tool_use_id: result.call_id.clone(),
                                is_error: result.is_error.then_some(true),
                                content: tool_content,
                                cache_control: None,
                            });
                        }
                    }
                }

                // Apply cache_control to the last content block if present
                if let Some(cache_hint) = &m.cache {
                    if let Some(last) = content.last_mut() {
                        last.set_cache_control(cache_control_from_hint(cache_hint));
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

        let (mut thinking, output_config) = self.thinking_for_request();

        // Forced tool use (`any` or a named tool) interacts with thinking.
        let forced_tool_choice = final_tool_choice
            .as_ref()
            .and_then(|tc| tc.get("type"))
            .is_some_and(|t| t == "any" || t == "tool");
        if forced_tool_choice {
            if self
                .parsed_model()
                .is_some_and(|m| m.rejects_forced_tool_choice())
            {
                return Err(LLMError::InvalidRequest(format!(
                    "Model {} does not support a forced tool_choice (`any` or a named tool)",
                    self.model
                )));
            }
            if self.thinking_class() == ThinkingClass::Budget && thinking.is_some() {
                log::debug!(target: "qmt_anthropic", "dropping thinking for {}: forced tool_choice", self.model);
                thinking = None;
            }
        }
        let output_config = if thinking.is_some() {
            output_config
        } else {
            None
        };
        let (temperature, top_p, top_k) = self.sampling_for(thinking.is_some());

        // Use sanitized system prompt for OAuth requests
        let mut sanitized_system = self.sanitize_system_prompt();
        if self.is_oauth() {
            self.validate_oauth_version()?;
            if let Some(first_user) = anthropic_messages
                .iter()
                .find(|message| message.role == "user")
            {
                let text = first_user
                    .content
                    .iter()
                    .find_map(|block| match block {
                        MessageContent::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .unwrap_or_default();
                if let Some(AnthropicSystemPrompt::Blocks(blocks)) = &mut sanitized_system {
                    blocks.insert(
                        0,
                        oauth_text_block(oauth_billing_header(text, self.oauth_version())),
                    );
                }
            }
        }

        let req_body = AnthropicCompleteRequest {
            messages: anthropic_messages,
            model: &self.model,
            max_tokens: Some(self.max_tokens),
            temperature,
            system: sanitized_system,
            stream: self.stream,
            top_p,
            top_k,
            tools: anthropic_tools,
            tool_choice: final_tool_choice,
            thinking,
            output_config,
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

    fn chat_stream_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut cfg = self.clone();
        cfg.stream = Some(true);
        cfg.chat_request(messages, tools)
    }

    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_anthropic_http_error(response)
    }

    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<ChatOutput, LLMError> {
        self.parse_chat_with_context(resp, Value::Null)
    }

    fn parse_chat_with_context(
        &self,
        resp: Response<Vec<u8>>,
        context: Value,
    ) -> Result<ChatOutput, LLMError> {
        let mut json_resp: AnthropicCompleteResponse = serde_json::from_slice(resp.body())
            .map_err(|e| LLMError::ResponseFormatError {
                message: format!("Failed to parse Anthropic response JSON: {e}"),
                raw_response: String::from_utf8_lossy(resp.body()).into_owned(),
            })?;

        // Strip tool prefix from tool names in response (for OAuth)
        if self.is_oauth() {
            let declared = self.tool_names_from_context(context)?;
            for content in &mut json_resp.content {
                if let Some(ref mut name) = content.name {
                    *name = Self::strip_tool_prefix(name, Some(&declared));
                }
            }
        }

        Ok(json_resp.into())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        self.chat_stream_parser_with_context(Value::Null)
    }

    fn chat_stream_parser_with_context(
        &self,
        context: Value,
    ) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(AnthropicStreamParser {
            oauth: self.is_oauth(),
            tool_names: self.tool_names_from_context(context)?,
            tool_state_buffer: HashMap::new(),
            thinking_state_buffer: HashMap::new(),
        }))
    }
}

struct AnthropicStreamParser {
    oauth: bool,
    /// Request-scoped wire-to-original tool-name mapping.
    tool_names: HashMap<String, String>,
    tool_state_buffer: HashMap<usize, AnthropicToolUseState>,
    thinking_state_buffer: HashMap<usize, AnthropicThinkingState>,
}

impl ChatStreamParser for AnthropicStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<querymt::chat::StreamChunk>, LLMError> {
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
                    "error" => {
                        return Err(map_anthropic_api_error(
                            stream_resp.error.as_ref(),
                            None,
                            None,
                            false,
                        )
                        .into());
                    }
                    "message_start" => {
                        if let Some(usage) = stream_resp.message.and_then(|m| m.usage) {
                            chunks.push(querymt::chat::StreamChunk::Usage(usage.into()));
                        }
                    }
                    "content_block_start" => {
                        if let (Some(index), Some(block)) =
                            (stream_resp.index, stream_resp.content_block)
                        {
                            if block.block_type == "tool_use" {
                                let name = block.name.unwrap_or_default();
                                let name = if self.oauth {
                                    Anthropic::strip_tool_prefix(&name, Some(&self.tool_names))
                                } else {
                                    name
                                };
                                let id = block.id.unwrap_or_default();

                                self.tool_state_buffer.insert(
                                    index,
                                    AnthropicToolUseState {
                                        id: id.clone(),
                                        name: name.clone(),
                                        arguments_buffer: String::new(),
                                    },
                                );

                                chunks.push(querymt::chat::StreamChunk::ToolUseStart {
                                    index,
                                    id,
                                    name,
                                });
                            } else if block.block_type == "thinking" {
                                // Created at block start so an empty block (display
                                // "omitted") still emits its signature at block stop.
                                self.thinking_state_buffer
                                    .insert(index, AnthropicThinkingState::default());
                            } else if block.block_type == "redacted_thinking"
                                && let Some(data) = block.data.filter(|d| !d.is_empty())
                            {
                                chunks.push(querymt::chat::StreamChunk::RedactedThinking(data));
                            }
                        }
                    }
                    "content_block_delta" => {
                        if let (Some(index), Some(delta)) = (stream_resp.index, stream_resp.delta) {
                            if let Some(text) = delta.text {
                                chunks.push(querymt::chat::StreamChunk::Text(text));
                            } else if let Some(thinking) = delta.thinking {
                                chunks.push(querymt::chat::StreamChunk::Thinking(thinking));
                            } else if let Some(signature) = delta.signature {
                                if let Some(state) = self.thinking_state_buffer.get_mut(&index) {
                                    state.signature.push_str(&signature);
                                }
                            } else if let Some(partial_json) = delta.partial_json {
                                if let Some(state) = self.tool_state_buffer.get_mut(&index) {
                                    state.arguments_buffer.push_str(&partial_json);
                                }

                                chunks.push(querymt::chat::StreamChunk::ToolUseInputDelta {
                                    index,
                                    partial_json,
                                });
                            }
                        }
                    }
                    "content_block_stop" => {
                        if let Some(index) = stream_resp.index {
                            if let Some(state) = self.tool_state_buffer.remove(&index) {
                                chunks.push(querymt::chat::StreamChunk::ToolUseComplete {
                                    index,
                                    tool_call: querymt::ToolCall {
                                        id: state.id,
                                        call_type: "function".to_string(),
                                        function: querymt::FunctionCall {
                                            name: state.name,
                                            arguments: if state.arguments_buffer.is_empty() {
                                                "{}".to_string()
                                            } else {
                                                state.arguments_buffer
                                            },
                                        },
                                    },
                                    extensions: Default::default(),
                                });
                            }

                            if let Some(state) = self.thinking_state_buffer.remove(&index)
                                && !state.signature.is_empty()
                            {
                                chunks.push(querymt::chat::StreamChunk::ThinkingSignature(
                                    state.signature,
                                ));
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = stream_resp.usage {
                            chunks.push(querymt::chat::StreamChunk::Usage(usage.into()));
                        }

                        if let Some(delta) = stream_resp.delta
                            && let Some(stop_reason) = delta.stop_reason
                        {
                            let finish_reason = Anthropic::map_stop_reason_with_details(
                                &stop_reason,
                                delta.stop_details.as_ref(),
                            );
                            chunks.push(querymt::chat::StreamChunk::Done { finish_reason });
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(chunks)
    }
}

/// Normalize Anthropic `error.type` tokens for kind lookup.
fn normalize_anthropic_error_token(token: &str) -> String {
    token.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

/// Map Anthropic `error.type` to a unified kind.
///
/// Anthropic documents: `invalid_request_error`, `authentication_error`,
/// `permission_error`, `not_found_error`, `request_too_large`, `rate_limit_error`,
/// `api_error`, `overloaded_error`.
fn anthropic_error_kind(error_type: &str) -> Option<ProviderErrorKind> {
    match error_type {
        "overloaded_error" | "overloaded" => Some(ProviderErrorKind::ServerOverloaded),
        "rate_limit_error" | "rate_limit_exceeded" | "rate_limited" => {
            Some(ProviderErrorKind::RateLimited)
        }
        "authentication_error" | "permission_error" => Some(ProviderErrorKind::Authentication),
        "invalid_request_error" | "invalid_request" | "not_found_error" | "request_too_large" => {
            Some(ProviderErrorKind::InvalidRequest)
        }
        "api_error" => Some(ProviderErrorKind::UnknownTransient),
        _ => None,
    }
}

fn map_anthropic_api_error(
    error: Option<&AnthropicApiErrorBody>,
    header_retry_after: Option<u64>,
    request_id: Option<&str>,
    unknown_transient: bool,
) -> ProviderFailure {
    let error_type = error.and_then(|e| e.error_type.clone());
    let message = error
        .and_then(|e| e.message.as_deref())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| "anthropic request failed".to_owned());

    let type_norm = error_type.as_deref().map(normalize_anthropic_error_token);
    let kind = type_norm
        .as_deref()
        .and_then(anthropic_error_kind)
        .unwrap_or(if unknown_transient {
            ProviderErrorKind::UnknownTransient
        } else {
            ProviderErrorKind::UnknownPermanent
        });

    let retry_after_secs = if kind == ProviderErrorKind::RateLimited {
        header_retry_after.or_else(|| parse_retry_after_from_message(&message))
    } else {
        header_retry_after
    };

    ProviderFailure::new(kind, message)
        .with_error_type(error_type)
        .with_request_id(request_id.map(str::to_owned))
        .with_retry_after_secs(retry_after_secs)
}

/// Classify an Anthropic HTTP error body.
fn classify_anthropic_http_error(response: &Response<Vec<u8>>) -> LLMError {
    let status = response.status().as_u16();
    let retry_after_secs = parse_retry_after(response.headers());
    let request_id = response
        .headers()
        .get("request-id")
        .or_else(|| response.headers().get("x-request-id"))
        .and_then(|value| value.to_str().ok());
    // Prefer structured `{"type":"error","error":{...}}` or top-level gateway errors.
    if let Ok(envelope) = serde_json::from_slice::<Value>(response.body()) {
        let error_value = envelope.get("error").cloned().or_else(|| {
            (envelope.get("message").is_some() && envelope.get("type").is_some())
                .then_some(envelope.clone())
        });

        if let Some(error_value) = error_value {
            let parsed: Option<AnthropicApiErrorBody> = serde_json::from_value(error_value).ok();
            if parsed
                .as_ref()
                .is_some_and(|e| e.error_type.is_some() || e.message.is_some())
            {
                let status_guess_transient = matches!(status, 429 | 500..=599);
                return map_anthropic_api_error(
                    parsed.as_ref(),
                    retry_after_secs,
                    request_id,
                    status_guess_transient,
                )
                .into();
            }
        }
    }

    classify_status_only(status, response.headers(), response.body())
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

    fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
        self.key_resolver.as_ref()
    }

    fn set_key_resolver(&mut self, resolver: Arc<dyn ApiKeyResolver>) {
        self.key_resolver = Some(resolver);
    }
}

mod factory;
pub use factory::create_http_factory;

#[cfg(test)]
mod tests {
    use super::*;

    /// The default qmtcode system prompt, shared with the agent crate's
    /// embedded profiles (see `crates/agent/examples/confs/single_coder.toml`).
    const DEFAULT_QMTCODE_SYSTEM_PROMPT: &str =
        include_str!("../../../agent/examples/prompts/default_system.txt");

    #[test]
    fn billing_header_matches_opencode_fixture() {
        assert_eq!(
            oauth_billing_header("hello world test message", "2.1.87"),
            "x-anthropic-billing-header: cc_version=2.1.87.6ff; cc_entrypoint=sdk-cli; cch=4ffc3;"
        );
    }

    /// Build a minimal `Anthropic` instance for use in unit tests.
    fn test_anthropic(api_key: &str) -> Anthropic {
        Anthropic {
            api_key: api_key.to_string(),
            auth_type: None,
            claude_code_version: None,
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
            reasoning_effort: None,
            reasoning_budget_tokens: None,
            adaptive_thinking: None,
            key_resolver: None,
        }
    }

    #[test]
    fn test_oauth_token_detection() {
        let anthropic = test_anthropic("sk-ant-oat01-abc123");
        assert_eq!(anthropic.determine_auth_type(), AuthType::OAuth);
    }

    #[test]
    fn test_api_key_detection() {
        let anthropic = test_anthropic("sk-ant-api03-xyz789");
        assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_explicit_auth_type_override() {
        // Even with an oat token, explicit auth_type should take precedence
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.auth_type = Some(AuthType::ApiKey);
        assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_fallback_to_api_key_for_unknown_format() {
        let anthropic = test_anthropic("sk-ant-unknown-format");
        // Should default to API key and print warning
        assert_eq!(anthropic.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_version_number_flexibility() {
        // Test with different version numbers
        let anthropic_oat99 = test_anthropic("sk-ant-oat99-future");
        assert_eq!(anthropic_oat99.determine_auth_type(), AuthType::OAuth);

        let anthropic_api15 = test_anthropic("sk-ant-api15-future");
        assert_eq!(anthropic_api15.determine_auth_type(), AuthType::ApiKey);
    }

    #[test]
    fn test_reasoning_budget_tokens_from_config_is_used_for_thinking_budget() {
        use querymt::chat::ChatMessage;

        let cfg = serde_json::json!({
            "api_key": "sk-ant-api03-test",
            "model": "claude-3-7-sonnet-20250219",
            "max_tokens": 2500,
            "reasoning_effort": "high",
            "reasoning_budget_tokens": 1024
        });

        let anthropic: Anthropic = serde_json::from_value(cfg)
            .expect("reasoning_budget_tokens should be accepted in Anthropic config");

        let messages = vec![
            ChatMessage::user()
                .text("How many r in strawberry?")
                .build(),
        ];
        let req = anthropic
            .chat_request(&messages, None)
            .expect("chat request should build");

        let body: serde_json::Value =
            serde_json::from_slice(req.body()).expect("request body should be valid JSON");
        assert_eq!(body["max_tokens"], serde_json::json!(2500));
        assert_eq!(body["thinking"]["type"], serde_json::json!("enabled"));
        assert_eq!(body["thinking"]["budget_tokens"], serde_json::json!(1024));
    }

    #[test]
    fn portable_instruction_hints_map_to_anthropic_cache_control() {
        let prompt: AnthropicSystemPrompt = serde_json::from_value(serde_json::json!([
            "legacy",
            {"text": "stable", "cache": {"ephemeral": {"ttl_seconds": 1800}}},
            {"type": "text", "text": "native", "cache_control": {"type": "ephemeral", "ttl": "5m"}, "cache": {"ephemeral": {"ttl_seconds": 1800}}},
            {"type": "text", "text": "native with portable hint", "cache": {"ephemeral": {"ttl_seconds": 300}}}
        ]))
        .unwrap();
        let AnthropicSystemPrompt::Blocks(blocks) = prompt else {
            panic!("blocks");
        };
        assert!(blocks[0].cache_control.is_none());
        assert_eq!(
            blocks[1].cache_control.as_ref().unwrap().ttl,
            Some(CacheTTL::OneHour)
        );
        assert_eq!(
            blocks[2].cache_control.as_ref().unwrap().ttl,
            Some(CacheTTL::FiveMinutes)
        );
        assert_eq!(blocks[1].text, "stable");
        assert_eq!(
            blocks[3].cache_control.as_ref().unwrap().ttl,
            Some(CacheTTL::FiveMinutes)
        );
    }

    #[test]
    fn portable_defaults_match_legacy_wire_controls() {
        let portable: AnthropicSystemPrompt = serde_json::from_value(serde_json::json!([
            {"text": "stable", "cache": {"ephemeral": {}}}
        ]))
        .unwrap();
        let legacy: AnthropicSystemPrompt = serde_json::from_value(serde_json::json!([
            {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral"}}
        ]))
        .unwrap();
        assert_eq!(
            serde_json::to_value(portable).unwrap(),
            serde_json::to_value(legacy).unwrap()
        );
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
    fn test_sanitize_system_prompt_oauth_no_system() {
        // OAuth with no system prompt → OAUTH_SYSTEM_PROMPT injected as the sole block
        let anthropic = test_anthropic("sk-ant-oat01-abc123");
        let sanitized = anthropic.sanitize_system_prompt();
        match sanitized {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
                assert!(blocks[0].cache_control.is_none());
            }
            other => panic!("Expected Blocks with one OAuth block, got {:?}", other),
        }
    }

    #[test]
    fn test_sanitize_system_prompt_oauth_text() {
        // OAuth with a plain Text prompt → OAUTH_SYSTEM_PROMPT prepended, original preserved
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.system = Some(AnthropicSystemPrompt::Text(
            "You are QueryMT assistant.".to_string(),
        ));
        let sanitized = anthropic.sanitize_system_prompt();
        match sanitized {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
                assert_eq!(blocks[1].text, "You are QueryMT assistant.");
            }
            other => panic!("Expected Blocks with two entries, got {:?}", other),
        }
    }

    #[test]
    fn test_sanitize_system_prompt_oauth_blocks() {
        // OAuth with existing Blocks → OAUTH_SYSTEM_PROMPT prepended, original blocks preserved
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.system = Some(AnthropicSystemPrompt::Blocks(vec![TextBlockParam {
            block_type: "text".to_string(),
            text: "You are QueryMT assistant.".to_string(),
            cache_control: Some(CacheControlEphemeral {
                control_type: "ephemeral".to_string(),
                ttl: None,
            }),
            citations: None,
        }]));
        let sanitized = anthropic.sanitize_system_prompt();
        match sanitized {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
                assert!(blocks[0].cache_control.is_none());
                assert_eq!(blocks[1].text, "You are QueryMT assistant.");
                // cache_control on the original block should be preserved
                assert!(blocks[1].cache_control.is_some());
            }
            other => panic!("Expected Blocks with two entries, got {:?}", other),
        }
    }

    #[test]
    fn test_sanitize_system_prompt_non_oauth_passthrough() {
        // Non-OAuth: system prompt returned unchanged, no blocks injected
        let mut anthropic = test_anthropic("sk-ant-api03-xyz789");
        anthropic.system = Some(AnthropicSystemPrompt::Text(
            "You are QueryMT assistant.".to_string(),
        ));
        let sanitized = anthropic.sanitize_system_prompt();
        assert_eq!(
            sanitized,
            Some(AnthropicSystemPrompt::Text(
                "You are QueryMT assistant.".to_string()
            ))
        );
    }

    #[test]
    fn sanitize_oauth_text_prunes_identity_paragraph() {
        // Identity in its own paragraph is removed along with the paragraph.
        let text = "You are QueryMT, a coding agent for software engineering tasks.\n\nUse the available tools to help the user accurately and efficiently.";
        assert_eq!(
            sanitize_oauth_text(text),
            "Use the available tools to help the user accurately and efficiently."
        );
    }

    #[test]
    fn sanitize_oauth_text_prunes_identity_line_within_paragraph() {
        // Only the identity line is removed; sibling lines in the same
        // paragraph survive.
        let text = "You are QueryMT, a coding agent for software engineering tasks.\nUse the available tools to help the user accurately and efficiently.";
        assert_eq!(
            sanitize_oauth_text(text),
            "Use the available tools to help the user accurately and efficiently."
        );
    }

    #[test]
    fn sanitize_oauth_text_keeps_env_metadata_next_to_powered_by() {
        // code_meta.jinja2 renders the identity line and `<env>` metadata in
        // one paragraph; only the identity line must be pruned.
        let text = "You are powered by anthropic/claude-sonnet-4-5-20250929.\n<env>\n  Is directory a git repo: yes\n  Platform: macos\n</env>";
        let sanitized = sanitize_oauth_text(text);
        assert!(!sanitized.contains("You are powered by"));
        assert!(sanitized.contains("<env>"));
        assert!(sanitized.contains("Platform: macos"));
    }

    #[test]
    fn sanitize_oauth_text_preserves_default_qmtcode_prompt_instructions() {
        let sanitized = sanitize_oauth_text(DEFAULT_QMTCODE_SYSTEM_PROMPT);
        assert!(!sanitized.contains("You are QueryMT,"));
        // Operational instructions and QueryMT references in guidance stay.
        assert!(sanitized.contains("# Task management"));
        assert!(sanitized.contains("# Code search policy"));
        assert!(sanitized.contains("# Tool usage policy"));
        assert!(sanitized.contains("answer from QueryMT docs"));
    }

    #[test]
    fn sanitize_system_prompt_oauth_prunes_identity_only_text_prompt() {
        // A prompt consisting solely of the identity line prunes to nothing;
        // only the injected OAuth identity block remains.
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.system = Some(AnthropicSystemPrompt::Text(
            "You are QueryMT, a coding agent for software engineering tasks.".to_string(),
        ));
        let sanitized = anthropic.sanitize_system_prompt();
        match sanitized {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
            }
            other => panic!("Expected single OAuth identity block, got {:?}", other),
        }
    }

    #[test]
    fn sanitize_system_prompt_oauth_drops_identity_only_blocks() {
        // Identity-only blocks are dropped; instruction blocks (with their
        // cache_control) are preserved alongside the injected identity block.
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.system = Some(AnthropicSystemPrompt::Blocks(vec![
            TextBlockParam {
                block_type: "text".to_string(),
                text: "You are QueryMT, a coding agent for software engineering tasks.".to_string(),
                cache_control: None,
                citations: None,
            },
            TextBlockParam {
                block_type: "text".to_string(),
                text: "# Code search policy\n- Use `search_text` first for content search."
                    .to_string(),
                cache_control: Some(CacheControlEphemeral {
                    control_type: "ephemeral".to_string(),
                    ttl: None,
                }),
                citations: None,
            },
        ]));
        let sanitized = anthropic.sanitize_system_prompt();
        match sanitized {
            Some(AnthropicSystemPrompt::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks[0].text, OAUTH_SYSTEM_PROMPT);
                assert!(blocks[1].text.starts_with("# Code search policy"));
                assert!(blocks[1].cache_control.is_some());
            }
            other => panic!("Expected Blocks with two entries, got {:?}", other),
        }
    }

    fn tool_response(name: &str) -> Response<Vec<u8>> {
        Response::builder()
            .status(200)
            .body(
                serde_json::to_vec(&serde_json::json!({
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": name, "input": {}}],
                    "stop_reason": "tool_use"
                }))
                .unwrap(),
            )
            .unwrap()
    }

    fn output_tool_name(output: ChatOutput) -> String {
        output
            .items
            .into_iter()
            .find_map(|item| match item {
                ChatOutputItem::FunctionCall(call) => Some(call.name),
                _ => None,
            })
            .expect("tool call")
    }

    #[test]
    fn prepared_context_restores_per_call_names_without_configured_tools() {
        let anthropic = test_anthropic("sk-ant-oat01-test");
        for name in [
            "GetWeather",
            "getWeather",
            "mcp_custom",
            "StructuredOutput",
            "structuredOutput",
        ] {
            let tools = [test_tool(name)];
            let prepared = anthropic
                .prepare_chat_request(&[], Some(&tools), false)
                .unwrap();
            let wire = anthropic.prefix_tool_name(name);
            let request: Value = serde_json::from_slice(prepared.request.body()).unwrap();
            assert_eq!(request["tools"][0]["name"], wire);
            let output = anthropic
                .parse_chat_with_context(tool_response(&wire), prepared.context)
                .unwrap();
            assert_eq!(output_tool_name(output), name);
        }
    }

    #[test]
    fn prepared_context_uses_overrides_and_survives_out_of_order_responses() {
        let mut anthropic = test_anthropic("sk-ant-oat01-test");
        anthropic.tools = Some(vec![test_tool("getWeather")]);
        let upper = [test_tool("GetWeather")];
        let first = anthropic
            .prepare_chat_request(&[], Some(&upper), false)
            .unwrap();
        let second = anthropic.prepare_chat_request(&[], None, false).unwrap();
        assert_eq!(
            output_tool_name(
                anthropic
                    .parse_chat_with_context(tool_response("mcp_GetWeather"), second.context,)
                    .unwrap()
            ),
            "getWeather"
        );
        assert_eq!(
            output_tool_name(
                anthropic
                    .parse_chat_with_context(tool_response("mcp_GetWeather"), first.context,)
                    .unwrap()
            ),
            "GetWeather"
        );

        let empty = anthropic
            .prepare_chat_request(&[], Some(&[]), false)
            .unwrap();
        assert_eq!(empty.context, serde_json::json!({}));
        let request: Value = serde_json::from_slice(empty.request.body()).unwrap();
        assert_eq!(request["tools"], serde_json::json!([]));
    }

    #[test]
    fn prepared_stream_parsers_own_independent_tool_mappings() {
        let anthropic = test_anthropic("sk-ant-oat01-test");
        let upper = [test_tool("GetWeather")];
        let lower = [test_tool("getWeather")];
        let a = anthropic
            .prepare_chat_request(&[], Some(&upper), true)
            .unwrap();
        let b = anthropic
            .prepare_chat_request(&[], Some(&lower), true)
            .unwrap();
        let request: Value = serde_json::from_slice(a.request.body()).unwrap();
        assert_eq!(request["stream"], true);
        // Exercise the same JSON boundary used by the plugin transport.
        let context = serde_json::from_slice(&serde_json::to_vec(&a.context).unwrap()).unwrap();
        let mut a = anthropic.chat_stream_parser_with_context(context).unwrap();
        let mut b = anthropic
            .chat_stream_parser_with_context(b.context)
            .unwrap();
        let frame = br#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"mcp_GetWeather"}}
"#;
        for (parser, expected) in [(&mut b, "getWeather"), (&mut a, "GetWeather")] {
            let chunks = parser.parse_chunk(frame).unwrap();
            assert!(
                matches!(&chunks[0], querymt::chat::StreamChunk::ToolUseStart { name, .. } if name == expected)
            );
        }
    }

    #[test]
    fn oauth_rejects_colliding_tool_names_before_sending() {
        let anthropic = test_anthropic("sk-ant-oat01-test");
        let tools = [test_tool("getWeather"), test_tool("GetWeather")];
        for stream in [false, true] {
            let error = anthropic
                .prepare_chat_request(&[], Some(&tools), stream)
                .err()
                .expect("collision");
            assert!(
                matches!(error, LLMError::InvalidRequest(message) if message.contains("mcp_GetWeather"))
            );
        }
        assert!(anthropic.chat_request(&[], Some(&tools)).is_err());
        let api_key = test_anthropic("sk-ant-api03-test");
        assert!(
            api_key
                .prepare_chat_request(&[], Some(&tools), false)
                .is_ok()
        );
    }

    fn test_tool(name: &str) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: name.to_string(),
                description: "test tool".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                strict: None,
            },
        }
    }

    #[test]
    fn strip_tool_prefix_preserves_unprefixed_names() {
        // Names without TOOL_PREFIX were never transformed: keep them exact.
        assert_eq!(
            Anthropic::strip_tool_prefix("GetWeather", None),
            "GetWeather"
        );
        assert_eq!(Anthropic::strip_tool_prefix("read_file", None), "read_file");
        assert_eq!(
            Anthropic::strip_tool_prefix("StructuredOutput", None),
            "StructuredOutput"
        );
    }

    #[test]
    fn strip_tool_prefix_restores_declared_capitalization() {
        let declared = HashMap::from([("mcp_GetWeather".to_string(), "GetWeather".to_string())]);

        // Prefixed names of declared tools are restored exactly.
        assert_eq!(
            Anthropic::strip_tool_prefix("mcp_GetWeather", Some(&declared)),
            "GetWeather"
        );
        // Undeclared tools keep the legacy first-character lowercasing.
        assert_eq!(
            Anthropic::strip_tool_prefix("mcp_get_weather", Some(&declared)),
            "get_weather"
        );
        // Prefixed StructuredOutput keeps its legacy handling.
        assert_eq!(
            Anthropic::strip_tool_prefix("mcp_StructuredOutput", Some(&declared)),
            "StructuredOutput"
        );
    }

    #[test]
    fn tool_name_prefixing_round_trips_declared_names() {
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.tools = Some(vec![test_tool("GetWeather")]);

        let wire = anthropic.prefix_tool_name("GetWeather");
        assert_eq!(wire, "mcp_GetWeather");
        assert_eq!(
            Anthropic::strip_tool_prefix(
                &wire,
                Some(&anthropic.declared_tool_names(None).unwrap())
            ),
            "GetWeather"
        );
    }

    #[test]
    fn parse_chat_restores_declared_tool_names() {
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.tools = Some(vec![test_tool("GetWeather")]);

        let body = serde_json::json!({
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "mcp_GetWeather", "input": {}},
                {"type": "text", "text": "Checking the weather."}
            ],
            "stop_reason": "tool_use"
        });
        let resp = Response::builder()
            .status(200)
            .body(serde_json::to_vec(&body).expect("json should serialize"))
            .expect("response should build");

        let output = anthropic.parse_chat(resp).expect("response should parse");
        let names: Vec<&str> = output
            .items
            .iter()
            .filter_map(|item| match item {
                ChatOutputItem::FunctionCall(call) => Some(call.name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["GetWeather"]);
    }

    #[test]
    fn streaming_oauth_tool_names_restore_declared_capitalization() {
        let mut anthropic = test_anthropic("sk-ant-oat01-abc123");
        anthropic.tools = Some(vec![test_tool("GetWeather")]);

        let lines = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"mcp_GetWeather"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
        ];
        let chunks = collect_chunks(&anthropic, &lines);

        assert!(
            matches!(
                &chunks[0],
                querymt::chat::StreamChunk::ToolUseStart { name, .. }
                if name == "GetWeather"
            ),
            "expected ToolUseStart with exact name, got {:?}",
            chunks[0]
        );
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

        let usage: Usage = serde_json::from_str::<AnthropicUsage>(json).unwrap().into();

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

    /// Feed a sequence of raw SSE lines through a fresh stream parser and
    /// collect all emitted `StreamChunk`s.
    fn collect_chunks(anthropic: &Anthropic, lines: &[&str]) -> Vec<querymt::chat::StreamChunk> {
        let mut parser = anthropic
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let mut out = Vec::new();
        for line in lines {
            let bytes = line.as_bytes();
            let parsed = parser.parse_chunk(bytes).unwrap();
            out.extend(parsed);
        }
        out
    }

    #[test]
    fn test_streaming_tool_call_assembled() {
        // Simulate the Anthropic SSE events for a single tool call streamed across
        // multiple chunks:
        //   content_block_start  → ToolUseStart
        //   content_block_delta  → ToolUseInputDelta (×2)
        //   content_block_stop   → ToolUseComplete  (with assembled JSON)
        //   message_delta        → Done
        let anthropic = test_anthropic("sk-ant-api03-test");

        let lines = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"read_file"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"foo.txt\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}"#,
        ];

        let chunks = collect_chunks(&anthropic, &lines);

        // ToolUseStart
        assert!(
            matches!(
                &chunks[0],
                querymt::chat::StreamChunk::ToolUseStart { index: 0, id, name }
                if id == "toolu_01" && name == "read_file"
            ),
            "expected ToolUseStart, got {:?}",
            chunks[0]
        );

        // Two ToolUseInputDelta chunks
        assert!(
            matches!(
                &chunks[1],
                querymt::chat::StreamChunk::ToolUseInputDelta { index: 0, partial_json }
                if partial_json == r#"{"path":"#
            ),
            "expected first ToolUseInputDelta, got {:?}",
            chunks[1]
        );
        assert!(
            matches!(
                &chunks[2],
                querymt::chat::StreamChunk::ToolUseInputDelta { index: 0, partial_json }
                if partial_json == r#""foo.txt"}"#
            ),
            "expected second ToolUseInputDelta, got {:?}",
            chunks[2]
        );

        // ToolUseComplete with fully assembled arguments
        match &chunks[3] {
            querymt::chat::StreamChunk::ToolUseComplete {
                index, tool_call, ..
            } => {
                assert_eq!(*index, 0);
                assert_eq!(tool_call.id, "toolu_01");
                assert_eq!(tool_call.function.name, "read_file");
                assert_eq!(tool_call.function.arguments, r#"{"path":"foo.txt"}"#);
            }
            other => panic!("expected ToolUseComplete, got {:?}", other),
        }

        // Done
        assert!(
            matches!(
                &chunks[4],
                querymt::chat::StreamChunk::Done { finish_reason }
                if *finish_reason == FinishReason::ToolCalls
            ),
            "expected Done, got {:?}",
            chunks[4]
        );

        // Parser state is per-stream and dropped with the parser instance.
    }

    #[test]
    fn test_streaming_multiple_tool_calls() {
        // Two tool calls at indices 0 and 1 (parallel tool calls)
        let anthropic = test_anthropic("sk-ant-api03-test");

        let lines = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"read_file"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\"}"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_02","name":"write_file"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"b.txt\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null}}"#,
        ];

        let chunks = collect_chunks(&anthropic, &lines);

        let complete_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| matches!(c, querymt::chat::StreamChunk::ToolUseComplete { .. }))
            .collect();

        assert_eq!(
            complete_chunks.len(),
            2,
            "expected 2 ToolUseComplete chunks"
        );

        // Verify each complete chunk has the right tool
        let names: std::collections::HashSet<&str> = complete_chunks
            .iter()
            .filter_map(|c| {
                if let querymt::chat::StreamChunk::ToolUseComplete { tool_call, .. } = c {
                    Some(tool_call.function.name.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(names.contains("read_file"));
        assert!(names.contains("write_file"));

        // Parser state is per-stream and dropped with the parser instance.
    }

    #[test]
    fn test_streaming_message_start_usage() {
        // Full "Hello" response from the example in the Anthropic docs.
        // message_start carries input_tokens; message_delta carries output_tokens.
        let anthropic = test_anthropic("sk-ant-api03-test");

        let lines = [
            r#"data: {"type":"message_start","message":{"id":"msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY","type":"message","role":"assistant","content":[],"model":"claude-opus-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"ping"}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"!"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
            r#"data: {"type":"message_stop"}"#,
        ];

        let chunks = collect_chunks(&anthropic, &lines);

        // First chunk must be Usage from message_start (input_tokens)
        match &chunks[0] {
            querymt::chat::StreamChunk::Usage(u) => {
                assert_eq!(u.input_tokens, 25);
                assert_eq!(u.output_tokens, 1);
            }
            other => panic!("expected Usage from message_start, got {:?}", other),
        }

        // Exactly two Usage chunks total: one from message_start, one from message_delta
        let usage_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| matches!(c, querymt::chat::StreamChunk::Usage(_)))
            .collect();
        assert_eq!(usage_chunks.len(), 2, "expected 2 Usage chunks");

        // The second Usage chunk comes from message_delta and carries output_tokens
        match usage_chunks[1] {
            querymt::chat::StreamChunk::Usage(u) => {
                assert_eq!(u.output_tokens, 15);
            }
            other => panic!("expected second Usage from message_delta, got {:?}", other),
        }

        // Text chunks should have been emitted
        let text: String = chunks
            .iter()
            .filter_map(|c| {
                if let querymt::chat::StreamChunk::Text(t) = c {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "Hello!");
    }

    #[test]
    fn test_streaming_usage_merge_max_gives_correct_totals() {
        // Verifies that applying Usage::merge_max across the two Usage chunks
        // emitted by the Anthropic provider yields the correct final totals.
        // This mirrors what transitions.rs now does for every streaming response.
        let anthropic = test_anthropic("sk-ant-api03-test");

        let lines = [
            r#"data: {"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","content":[],"model":"claude-opus-4-6","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}"#,
        ];

        let chunks = collect_chunks(&anthropic, &lines);

        // Collect all Usage chunks and fold them with merge_max (same logic as transitions.rs)
        let merged = chunks.iter().fold(None::<Usage>, |acc, chunk| {
            if let querymt::chat::StreamChunk::Usage(u) = chunk {
                Some(match acc {
                    Some(prev) => prev.merge_max(u.clone()),
                    None => u.clone(),
                })
            } else {
                acc
            }
        });

        let merged = merged.expect("at least one Usage chunk expected");
        assert_eq!(
            merged.input_tokens, 25,
            "input_tokens must come from message_start"
        );
        assert_eq!(
            merged.output_tokens, 15,
            "output_tokens must be the cumulative value from message_delta"
        );
        assert_eq!(merged.cache_read, 0);
        assert_eq!(merged.cache_write, 0);
    }

    #[test]
    fn test_streaming_text_only_no_tool_complete() {
        // Pure text response — no ToolUseComplete should be emitted
        let anthropic = test_anthropic("sk-ant-api03-test");

        let lines = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello!"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null}}"#,
        ];

        let chunks = collect_chunks(&anthropic, &lines);

        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, querymt::chat::StreamChunk::ToolUseComplete { .. })),
            "no ToolUseComplete expected for a text-only response"
        );
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, querymt::chat::StreamChunk::Text(t) if t == "Hello!")),
            "expected a Text chunk"
        );
        // Parser state is per-stream and dropped with the parser instance.
    }

    #[test]
    fn classify_http_top_level_gateway_error_is_classified() {
        let response = Response::builder()
            .status(503)
            .body(br#"{"type":"api_error","message":"gateway busy"}"#.to_vec())
            .unwrap();
        let error = classify_anthropic_http_error(&response);
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::UnknownTransient
                    && failure.message() == "gateway busy"
                    && failure.error_type() == Some("api_error")
                    && failure.code().is_none()
        ));
    }

    #[test]
    fn classify_http_missing_message_uses_safe_fallback() {
        let response = Response::builder()
            .status(500)
            .body(br#"{"type":"error","error":{"type":"api_error","private":"secret"}}"#.to_vec())
            .unwrap();
        let error = classify_anthropic_http_error(&response);
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.message() == "anthropic request failed"
                    && !failure.message().contains("private")
        ));
    }

    #[test]
    fn classify_http_overloaded_is_retryable() {
        let response = Response::builder()
            .status(529)
            .body(
                br#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
                    .to_vec(),
            )
            .unwrap();
        let error = classify_anthropic_http_error(&response);
        assert!(error.is_retryable());
        match &error {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::ServerOverloaded);
                assert_eq!(failure.error_type(), Some("overloaded_error"));
                assert_eq!(failure.code(), None);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn classify_http_invalid_request_is_permanent() {
        let response = Response::builder()
            .status(400)
            .body(
                br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: text content blocks must be non-empty"}}"#
                    .to_vec(),
            )
            .unwrap();
        let error = classify_anthropic_http_error(&response);
        assert!(!error.is_retryable());
        match &error {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::InvalidRequest);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn classify_http_rate_limit_is_retryable() {
        let response = Response::builder()
            .status(429)
            .header(http::header::RETRY_AFTER, "12")
            .header("request-id", "req-ant-http")
            .body(
                br#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limited"}}"#
                    .to_vec(),
            )
            .unwrap();
        let error = classify_anthropic_http_error(&response);
        assert!(error.is_retryable());
        assert_eq!(error.retry_after_secs(), Some(12));
        match &error {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::RateLimited);
                assert_eq!(failure.request_id(), Some("req-ant-http"));
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn streaming_error_event_overloaded_is_retryable() {
        let anthropic = test_anthropic("sk-ant-api03-test");
        let mut parser = anthropic.chat_stream_parser().unwrap();
        let line =
            br#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}
"#;
        let err = parser
            .parse_chunk(line)
            .expect_err("error event must fail the stream");
        assert!(err.is_retryable());
        match err {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::ServerOverloaded);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn streaming_api_error_is_transient() {
        let anthropic = test_anthropic("sk-ant-api03-test");
        let mut parser = anthropic.chat_stream_parser().unwrap();
        let line = br#"data: {"type":"error","error":{"type":"api_error","message":"Internal server error"}}
"#;
        let error = parser
            .parse_chunk(line)
            .expect_err("api error event must fail the stream");

        assert!(error.is_retryable());
        match error {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::UnknownTransient);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn streaming_error_event_invalid_request_is_permanent() {
        let anthropic = test_anthropic("sk-ant-api03-test");
        let mut parser = anthropic.chat_stream_parser().unwrap();
        let line = br#"data: {"type":"error","error":{"type":"invalid_request_error","message":"bad request"}}
"#;
        let err = parser
            .parse_chunk(line)
            .expect_err("error event must fail the stream");
        assert!(!err.is_retryable());
        match err {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::InvalidRequest);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    // ---------------------------------------------------------------------
    // Thinking, sampling, tool-choice, replay and usage (anthropic-thinking-usage)
    // ---------------------------------------------------------------------

    fn anthropic_for(model: &str, effort: Option<ReasoningEffort>, max_tokens: u32) -> Anthropic {
        let mut a = test_anthropic("sk-ant-api03-test");
        a.model = model.to_string();
        a.reasoning_effort = effort;
        a.max_tokens = max_tokens;
        a.temperature = None;
        a
    }

    fn body_of(a: &Anthropic, messages: &[ChatMessage]) -> Value {
        let req = a.chat_request(messages, None).expect("request builds");
        serde_json::from_slice(req.body()).expect("valid JSON")
    }

    fn options_body(a: &Anthropic, options: &GenerationOptions, stream: bool) -> Value {
        let prepared = a
            .prepare_chat_request_with_options(&user_msgs(), None, stream, options)
            .expect("request builds");
        serde_json::from_slice(prepared.request.body()).expect("valid JSON")
    }

    #[test]
    fn provider_reports_per_request_option_support() {
        let a = anthropic_for("claude-sonnet-4-5", None, 32_000);
        assert_eq!(
            a.supported_generation_options(),
            GenerationOptionSupport::ALL
        );
    }

    #[test]
    fn max_tokens_override_applies_to_one_request_only() {
        let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        let options = GenerationOptions::default().with_max_tokens(128_000);

        assert_eq!(options_body(&a, &options, false)["max_tokens"], 128_000);
        assert_eq!(options_body(&a, &options, true)["max_tokens"], 128_000);
        // The next request, made without options, uses the configured value.
        assert_eq!(
            options_body(&a, &GenerationOptions::NONE, false)["max_tokens"],
            32_000
        );
        assert_eq!(a.max_tokens, 32_000);
    }

    #[test]
    fn empty_options_produce_an_identical_request() {
        let a = anthropic_for("claude-opus-4-8", Some(ReasoningEffort::High), 32_000);
        let plain = a
            .prepare_chat_request(&user_msgs(), None, false)
            .unwrap()
            .request;
        let with_none = a
            .prepare_chat_request_with_options(&user_msgs(), None, false, &GenerationOptions::NONE)
            .unwrap()
            .request;
        assert_eq!(plain.body(), with_none.body());
    }

    #[test]
    fn effort_override_changes_adaptive_output_config() {
        let a = anthropic_for("claude-sonnet-5-5", Some(ReasoningEffort::High), 32_000);
        let low = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Low);

        let b = options_body(&a, &low, false);
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "low");
        assert_eq!(body(&a)["output_config"]["effort"], "high");
    }

    #[test]
    fn effort_override_enables_thinking_when_none_configured() {
        let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        assert!(body(&a).get("thinking").is_none());

        let options = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Medium);
        let b = options_body(&a, &options, false);
        assert_eq!(b["output_config"]["effort"], "medium");
    }

    #[test]
    fn effort_override_replaces_budget_for_budget_models() {
        let a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
        assert_eq!(body(&a)["thinking"]["budget_tokens"], 16_000);

        let low = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Low);
        assert_eq!(
            options_body(&a, &low, false)["thinking"]["budget_tokens"],
            1_024
        );
    }

    #[test]
    fn effort_override_beats_configured_explicit_budget() {
        let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
        a.reasoning_budget_tokens = Some(12_000);
        assert_eq!(body(&a)["thinking"]["budget_tokens"], 12_000);

        let low = GenerationOptions::default().with_reasoning_effort(ReasoningEffort::Low);
        assert_eq!(
            options_body(&a, &low, false)["thinking"]["budget_tokens"],
            1_024
        );
        // Without an effort override the configured budget still applies.
        let tokens = GenerationOptions::default().with_max_tokens(64_000);
        assert_eq!(
            options_body(&a, &tokens, false)["thinking"]["budget_tokens"],
            12_000
        );
    }

    #[test]
    fn max_tokens_override_refits_the_thinking_budget() {
        let a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 20_000);
        assert_eq!(body(&a)["thinking"]["budget_tokens"], 10_000);

        let larger = GenerationOptions::default().with_max_tokens(64_000);
        assert_eq!(
            options_body(&a, &larger, false)["thinking"]["budget_tokens"],
            16_000
        );

        // A cap too small for the minimum budget omits thinking entirely.
        let tiny = GenerationOptions::default().with_max_tokens(1_500);
        let b = options_body(&a, &tiny, false);
        assert!(b.get("thinking").is_none());
        assert_eq!(b["max_tokens"], 1_500);
    }

    fn user_msgs() -> Vec<ChatMessage> {
        vec![ChatMessage::user().text("hi").build()]
    }

    fn body(a: &Anthropic) -> Value {
        body_of(a, &user_msgs())
    }

    #[test]
    fn adaptive_thinking_config_field_in_schema_and_factory() {
        let schema = serde_json::to_value(schemars::schema_for!(Anthropic)).unwrap();
        assert!(schema["properties"]["adaptive_thinking"].is_object());
        let cfg = serde_json::json!({
            "api_key": "sk-ant-api03-test",
            "model": "my-proxy-model",
            "max_tokens": 4096,
            "adaptive_thinking": true
        });
        let a: Anthropic = serde_json::from_value(cfg).unwrap();
        assert_eq!(a.adaptive_thinking, Some(true));
        let none: Anthropic = serde_json::from_value(serde_json::json!({
            "api_key": "k", "model": "m", "max_tokens": 1
        }))
        .unwrap();
        assert_eq!(none.adaptive_thinking, None);
    }

    #[test]
    fn adaptive_override_applies_in_both_directions() {
        let mut a = anthropic_for("my-proxy-model", None, 32_000);
        assert_eq!(a.thinking_class(), ThinkingClass::Budget);
        a.adaptive_thinking = Some(true);
        assert_eq!(a.thinking_class(), ThinkingClass::OptInAdaptive);
        assert_eq!(body(&a)["thinking"]["type"], "adaptive");

        let mut b = anthropic_for("claude-opus-4-8", None, 32_000);
        assert_eq!(b.thinking_class(), ThinkingClass::OptInAdaptive);
        b.adaptive_thinking = Some(false);
        assert_eq!(b.thinking_class(), ThinkingClass::Budget);
        assert!(body(&b).get("thinking").is_none());
    }

    #[test]
    fn predicates_ignore_the_override() {
        // Override forces budget, but the model still rejects sampling params.
        let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        a.adaptive_thinking = Some(false);
        a.temperature = Some(0.2);
        a.top_p = Some(0.99);
        let b = body(&a);
        assert!(b.get("temperature").is_none());
        assert!(b.get("top_p").is_none());

        // Override forces adaptive on an old model: forced tool choice allowed.
        let mut c = anthropic_for("claude-sonnet-4-5", None, 32_000);
        c.adaptive_thinking = Some(true);
        c.tools = Some(vec![test_tool("x")]);
        c.tool_choice = Some(ToolChoice::Tool("x".into()));
        assert!(c.chat_request(&user_msgs(), None).is_ok());
    }

    #[test]
    fn auto_effort_by_class() {
        let b = body(&anthropic_for("claude-sonnet-5-5", None, 32_000));
        assert!(b.get("thinking").is_none());
        assert!(b.get("output_config").is_none());

        let b = body(&anthropic_for("claude-opus-4-8", None, 32_000));
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert!(b["thinking"].get("budget_tokens").is_none());
        assert!(b.get("output_config").is_none());

        let b = body(&anthropic_for("claude-sonnet-4-5", None, 32_000));
        assert!(b.get("thinking").is_none());
        assert!(b.get("output_config").is_none());
    }

    #[test]
    fn explicit_effort_by_class() {
        for model in ["claude-opus-5", "claude-opus-4-6", "claude-sonnet-5-5"] {
            for (effort, name) in [
                (ReasoningEffort::Low, "low"),
                (ReasoningEffort::Medium, "medium"),
                (ReasoningEffort::High, "high"),
                (ReasoningEffort::Max, "max"),
            ] {
                let b = body(&anthropic_for(model, Some(effort), 32_000));
                assert_eq!(b["thinking"]["type"], "adaptive", "{model}");
                assert!(b["thinking"].get("budget_tokens").is_none());
                assert_eq!(b["output_config"]["effort"], name, "{model}");
            }
        }
        let b = body(&anthropic_for(
            "claude-sonnet-4-5",
            Some(ReasoningEffort::Medium),
            32_000,
        ));
        assert_eq!(b["thinking"]["type"], "enabled");
        assert_eq!(b["thinking"]["budget_tokens"], 8_000);
        assert!(b.get("output_config").is_none());
    }

    #[test]
    fn budgets_are_fitted_to_max_tokens() {
        let b = body(&anthropic_for(
            "claude-sonnet-4-5",
            Some(ReasoningEffort::Max),
            32_000,
        ));
        assert_eq!(b["thinking"]["budget_tokens"], 16_000);

        let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
        a.reasoning_budget_tokens = Some(20_000);
        assert_eq!(body(&a)["thinking"]["budget_tokens"], 16_000);

        let b = body(&anthropic_for(
            "claude-sonnet-4-5",
            Some(ReasoningEffort::Low),
            1_500,
        ));
        assert!(b.get("thinking").is_none());

        // Minimum budget applies when the configured budget is tiny.
        let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::Low), 4_000);
        a.reasoning_budget_tokens = Some(100);
        assert_eq!(body(&a)["thinking"]["budget_tokens"], 1_024);
    }

    #[test]
    fn sampling_filter_cases() {
        // Newer model: nothing is sent.
        let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        a.temperature = Some(0.2);
        a.top_p = Some(0.9);
        a.top_k = Some(40);
        let b = body(&a);
        assert!(b.get("temperature").is_none());
        assert!(b.get("top_p").is_none());
        assert!(b.get("top_k").is_none());

        // Opt-in adaptive in auto: thinking is sent, temperature is dropped.
        let mut a = anthropic_for("claude-sonnet-4-6", None, 32_000);
        a.temperature = Some(0.2);
        let b = body(&a);
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert!(b.get("temperature").is_none());

        // Budget model without thinking keeps configured values.
        let mut a = anthropic_for("claude-sonnet-4-5", None, 32_000);
        a.temperature = Some(0.2);
        a.top_k = Some(40);
        let b = body(&a);
        assert!((b["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert_eq!(b["top_k"], 40);

        // Budget model with thinking: top_p only within 0.95..=1.
        let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
        a.temperature = Some(0.2);
        a.top_k = Some(40);
        a.top_p = Some(0.9);
        let b = body(&a);
        assert_eq!(b["thinking"]["type"], "enabled");
        assert!(b.get("top_p").is_none());
        assert!(b.get("temperature").is_none());
        assert!(b.get("top_k").is_none());
        a.top_p = Some(0.97);
        let b = body(&a);
        assert!((b["top_p"].as_f64().unwrap() - 0.97).abs() < 1e-6);

        // Opus 4.7 rejects sampling params too, even without opt-in thinking.
        let mut a = anthropic_for("claude-opus-4-7", None, 32_000);
        a.temperature = Some(0.5);
        assert!(body(&a).get("temperature").is_none());
    }

    #[test]
    fn forced_tool_choice_rules() {
        let tools = vec![test_tool("x")];

        // Budget model: thinking dropped, tool_choice kept.
        let mut a = anthropic_for("claude-sonnet-4-5", Some(ReasoningEffort::High), 32_000);
        a.tools = Some(tools.clone());
        a.tool_choice = Some(ToolChoice::Any);
        let b = body(&a);
        assert_eq!(b["tool_choice"]["type"], "any");
        assert!(b.get("thinking").is_none());
        assert!(b.get("output_config").is_none());

        // Model that rejects forced tool use: error before sending.
        let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        a.tools = Some(tools.clone());
        a.tool_choice = Some(ToolChoice::Tool("x".into()));
        let err = a.chat_request(&user_msgs(), None).unwrap_err();
        assert!(matches!(err, LLMError::InvalidRequest(ref m) if m.contains("claude-sonnet-5-5")));

        // Other adaptive model: sent unchanged.
        let mut a = anthropic_for("claude-opus-4-8", None, 32_000);
        a.tools = Some(tools.clone());
        a.tool_choice = Some(ToolChoice::Any);
        let b = body(&a);
        assert_eq!(b["tool_choice"]["type"], "any");
        assert_eq!(b["thinking"]["type"], "adaptive");

        // `auto` is never treated as forced.
        let mut a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        a.tools = Some(tools);
        a.tool_choice = Some(ToolChoice::Auto);
        assert!(a.chat_request(&user_msgs(), None).is_ok());
    }

    fn reasoning_item(text: &str, signature: Option<&str>) -> ChatOutputItem {
        ChatOutputItem::Reasoning(ChatReasoningItem {
            id: None,
            summary: Vec::new(),
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ChatReasoningPart::text(text)]
            },
            encrypted_content: None,
            signature: signature.map(str::to_string),
            status: None,
            extensions: Default::default(),
        })
    }

    fn call_item(id: &str) -> ChatOutputItem {
        ChatOutputItem::FunctionCall(ChatFunctionCallItem {
            item_id: None,
            call_id: id.to_string(),
            name: "read_file".into(),
            arguments: "{}".into(),
            status: None,
            extensions: Default::default(),
        })
    }

    fn replay_assistant_content(items: Vec<ChatOutputItem>) -> Vec<Value> {
        let output = ChatOutput {
            items,
            ..ChatOutput::default()
        };
        let messages = vec![
            ChatMessage::user().text("go").build(),
            ChatMessage::from_assistant_output(output),
        ];
        let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        let b = body_of(&a, &messages);
        b["messages"][1]["content"].as_array().unwrap().clone()
    }

    #[test]
    fn replay_sends_empty_signed_thinking_before_tool_use() {
        let content =
            replay_assistant_content(vec![reasoning_item("", Some("S")), call_item("toolu_1")]);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "");
        assert_eq!(content[0]["signature"], "S");
        assert_eq!(content[1]["type"], "tool_use");
    }

    #[test]
    fn replay_keeps_multiple_blocks_in_order_and_skips_unsigned() {
        let content = replay_assistant_content(vec![
            reasoning_item("one", Some("S1")),
            reasoning_item("unsigned", None),
            ChatOutputItem::Message(ChatMessageItem {
                id: None,
                role: ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![ChatMessagePart::Text {
                    text: "progress".into(),
                    annotations: Vec::new(),
                    extensions: Default::default(),
                }],
                extensions: Default::default(),
            }),
            reasoning_item("two", Some("S2")),
        ]);
        let thinking: Vec<&Value> = content.iter().filter(|c| c["type"] == "thinking").collect();
        assert_eq!(thinking.len(), 2);
        assert_eq!(thinking[0]["signature"], "S1");
        assert_eq!(thinking[0]["thinking"], "one");
        assert_eq!(thinking[1]["signature"], "S2");
        assert_eq!(thinking[1]["thinking"], "two");
    }

    #[test]
    fn replay_sends_redacted_thinking_before_tool_use() {
        let content = replay_assistant_content(vec![
            redacted_reasoning_item("D".to_string()),
            call_item("toolu_1"),
        ]);
        assert_eq!(content[0]["type"], "redacted_thinking");
        assert_eq!(content[0]["data"], "D");
        assert_eq!(content[1]["type"], "tool_use");
        // Unmarked encrypted content from another provider is not replayed.
        let mut other = reasoning_item("", None);
        if let ChatOutputItem::Reasoning(r) = &mut other {
            r.encrypted_content = Some("foreign".into());
        }
        let content = replay_assistant_content(vec![other, call_item("toolu_2")]);
        assert!(content.iter().all(|c| c["type"] != "redacted_thinking"));
    }

    #[test]
    fn portable_projection_still_strips_signature_and_redacted_data() {
        let output = ChatOutput {
            items: vec![
                reasoning_item("t", Some("S")),
                redacted_reasoning_item("D".into()),
            ],
            ..ChatOutput::default()
        };
        let portable = output.clone().into_portable();
        let dump = serde_json::to_string(&portable).unwrap();
        assert!(!dump.contains("\"S\""), "{dump}");
        assert!(!dump.contains("\"D\""), "{dump}");
    }

    fn sse(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|l| format!("data: {l}\n")).collect()
    }

    fn stream_output(lines: &[&str]) -> (ChatOutput, Vec<querymt::chat::StreamChunk>) {
        let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        let data = sse(lines);
        let refs: Vec<&str> = data.iter().map(String::as_str).collect();
        let chunks = collect_chunks(&a, &refs);
        let mut acc = querymt::chat::ChatStreamAccumulator::new();
        for c in &chunks {
            acc.push(c).unwrap();
        }
        (acc.output(), chunks)
    }

    #[test]
    fn streaming_omitted_thinking_block_emits_signature() {
        let (output, chunks) = stream_output(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read_file"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}"#,
        ]);
        assert!(
            chunks.iter().any(
                |c| matches!(c, querymt::chat::StreamChunk::ThinkingSignature(s) if s == "SIG")
            )
        );
        let ChatOutputItem::Reasoning(r) = &output.items[0] else {
            panic!("expected reasoning first: {:?}", output.items);
        };
        assert_eq!(r.signature.as_deref(), Some("SIG"));
        assert!(r.visible_text().is_empty());
        assert!(matches!(output.items[1], ChatOutputItem::FunctionCall(_)));
    }

    #[test]
    fn streaming_consecutive_thinking_blocks_are_separate_items() {
        let (output, _) = stream_output(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"one"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"S1"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"progress"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"two"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"signature_delta","signature":"S2"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
        ]);
        let sigs: Vec<_> = output
            .items
            .iter()
            .filter_map(|i| match i {
                ChatOutputItem::Reasoning(r) => r.signature.clone(),
                _ => None,
            })
            .collect();
        assert_eq!(sigs, vec!["S1", "S2"]);
    }

    #[test]
    fn streaming_redacted_thinking_block() {
        let (output, chunks) = stream_output(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"OPAQUE"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read_file"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
        ]);
        assert!(chunks.iter().any(
            |c| matches!(c, querymt::chat::StreamChunk::RedactedThinking(d) if d == "OPAQUE")
        ));
        let ChatOutputItem::Reasoning(r) = &output.items[0] else {
            panic!("expected reasoning");
        };
        assert_eq!(r.encrypted_content.as_deref(), Some("OPAQUE"));
        assert!(r.visible_text().is_empty());
        assert!(!format!("{chunks:?}").contains("OPAQUE"));
    }

    fn parse_response(json: Value) -> ChatOutput {
        let a = anthropic_for("claude-sonnet-5-5", None, 32_000);
        let resp = Response::builder()
            .status(200)
            .body(serde_json::to_vec(&json).unwrap())
            .unwrap();
        a.parse_chat(resp).expect("response parses")
    }

    #[test]
    fn non_streaming_items_in_content_order_with_signatures() {
        let output = parse_response(serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "one", "signature": "S1"},
                {"type": "redacted_thinking", "data": "D"},
                {"type": "thinking", "thinking": "", "signature": "S2"},
                {"type": "text", "text": "answer"},
                {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 5, "output_tokens": 600,
                      "output_tokens_details": {"thinking_tokens": 500}}
        }));
        assert_eq!(output.items.len(), 5);
        let ChatOutputItem::Reasoning(r0) = &output.items[0] else {
            panic!()
        };
        assert_eq!(r0.signature.as_deref(), Some("S1"));
        assert_eq!(r0.visible_text(), "one");
        let ChatOutputItem::Reasoning(r1) = &output.items[1] else {
            panic!()
        };
        assert_eq!(r1.encrypted_content.as_deref(), Some("D"));
        assert!(r1.signature.is_none());
        let ChatOutputItem::Reasoning(r2) = &output.items[2] else {
            panic!()
        };
        assert_eq!(r2.signature.as_deref(), Some("S2"));
        assert!(matches!(output.items[3], ChatOutputItem::Message(_)));
        assert!(matches!(output.items[4], ChatOutputItem::FunctionCall(_)));
        assert_eq!(output.text().as_deref(), Some("answer"));
        assert_eq!(output.finish_reason, Some(FinishReason::ToolCalls));
        let usage = output.usage.unwrap();
        assert_eq!(usage.output_tokens, 100);
        assert_eq!(usage.reasoning_tokens, 500);
    }

    #[test]
    fn non_streaming_thinking_round_trips_through_replay() {
        let output = parse_response(serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "", "signature": "S"},
                {"type": "redacted_thinking", "data": "D"},
                {"type": "tool_use", "id": "t1", "name": "read_file", "input": {}}
            ],
            "stop_reason": "tool_use"
        }));
        let content = replay_assistant_content(output.items);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "S");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "D");
        assert_eq!(content[2]["type"], "tool_use");
    }

    #[test]
    fn usage_maps_thinking_tokens_streaming_and_non_streaming() {
        let usage: AnthropicUsage = serde_json::from_str(
            r#"{"input_tokens":12,"cache_creation_input_tokens":1495,"cache_read_input_tokens":3,
                "output_tokens":1024,"service_tier":"standard"}"#,
        )
        .unwrap();
        let usage: Usage = usage.into();
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 1024);
        assert_eq!(usage.cache_write, 1495);
        assert_eq!(usage.cache_read, 3);
        assert_eq!(usage.reasoning_tokens, 0);

        let (output, _) = stream_output(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":7,"output_tokens":1}}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":32000,"output_tokens_details":{"thinking_tokens":31950}}}"#,
        ]);
        let usage = output.usage.unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.reasoning_tokens, 31_950);
        assert_eq!(output.finish_reason, Some(FinishReason::Length));
    }

    #[test]
    fn stop_reasons_map_identically_on_both_paths() {
        let cases = [
            ("end_turn", FinishReason::Stop),
            ("stop_sequence", FinishReason::Stop),
            ("tool_use", FinishReason::ToolCalls),
            ("max_tokens", FinishReason::Length),
            ("model_context_window_exceeded", FinishReason::Length),
            ("refusal", FinishReason::ContentFilter),
            ("pause_turn", FinishReason::Other),
            ("something_new", FinishReason::Unknown),
        ];
        for (raw, expected) in cases {
            assert_eq!(Anthropic::map_stop_reason(raw), expected, "{raw}");

            let output = parse_response(serde_json::json!({
                "content": [{"type": "text", "text": "x"}],
                "stop_reason": raw,
                "stop_details": {"category": "cyber"}
            }));
            assert_eq!(output.finish_reason, Some(expected), "non-streaming {raw}");

            let delta = format!(
                r#"{{"type":"message_delta","delta":{{"stop_reason":"{raw}","stop_details":{{"category":"cyber"}}}},"usage":{{"output_tokens":1}}}}"#
            );
            let (out, _) = stream_output(&[delta.as_str()]);
            assert_eq!(out.finish_reason, Some(expected), "streaming {raw}");
        }
    }
}
