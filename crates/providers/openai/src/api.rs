use base64::Engine as _;
use either::*;
use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use querymt::{
    FunctionCall, ToolCall, Usage,
    chat::{
        ChatFunctionCallItem, ChatInputPart, ChatMessage, ChatMessageItem, ChatMessagePart,
        ChatMessagePartDelta, ChatOpaqueItem, ChatOpaquePart, ChatOutput, ChatOutputItem,
        ChatOutputProvenance, ChatOutputStatus, ChatReasoningItem, ChatReasoningPart, ChatRole,
        ChatTextAnnotation, Extensions, FinishReason, MediaKind, MediaPart, MediaSource,
        ReasoningEffort, ReasoningPartKind, StreamChunk, StructuredOutputFormat,
        StructuredStreamEvent, Tool, ToolChoice, ToolResultPart,
    },
    error::{
        LLMError, ProviderErrorKind, ProviderFailure, extract_retry_after_from_json,
        parse_retry_after, parse_retry_after_from_message,
    },
    handle_http_error,
    stt::{SttRequest, SttResponse},
    tts::{TtsRequest, TtsResponse},
};
use schemars::{Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::HashMap;
use url::Url;

use heck::ToSnakeCase;

use crate::AuthType;

fn should_snakecase_extra_body(base_url: &Url) -> bool {
    // Why: `extra_body` is an untyped string->JSON map that gets flattened into the
    // request body, so serde can't apply `rename_all = "snake_case"` to its keys.
    //
    // The OpenAI API expects snake_case parameter names (e.g. `prompt_cache_key`),
    // but some internal/default heuristics and user configs use camelCase
    // (e.g. `promptCacheKey`). We normalize here so callers can supply either.
    //
    // Scope: only apply this normalization for the real OpenAI API. Many
    // OpenAI-compatible providers accept different parameter names and/or casing,
    // so rewriting keys could break them.
    matches!(base_url.host_str(), Some("api.openai.com"))
}

fn normalize_extra_body_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(normalize_extra_body_map(map)),
        Value::Array(arr) => {
            Value::Array(arr.into_iter().map(normalize_extra_body_value).collect())
        }
        other => other,
    }
}

fn normalize_extra_body_map(map: Map<String, Value>) -> Map<String, Value> {
    // Two-pass to prefer keys that are already snake_case.
    let entries: Vec<(String, Value)> = map.into_iter().collect();
    let mut out = Map::with_capacity(entries.len());

    for (k, v) in &entries {
        let nk = k.to_snake_case();
        if &nk == k {
            out.insert(k.clone(), normalize_extra_body_value(v.clone()));
        }
    }

    for (k, v) in entries {
        let nk = k.to_snake_case();
        if nk != k && !out.contains_key(&nk) {
            out.insert(nk, normalize_extra_body_value(v));
        }
    }

    out
}

pub fn url_schema(_gen: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "format": "uri"
    })
}

/// Individual message in an OpenAI chat conversation.
#[derive(Serialize, Debug)]
struct OpenAIChatMessage<'a> {
    #[allow(dead_code)]
    role: Cow<'a, str>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "either::serde_untagged_optional"
    )]
    content: Option<Either<Vec<MessageContent<'a>>, Cow<'a, str>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIFunctionCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<Cow<'a, str>>,
}

#[derive(Serialize, Debug)]
struct OpenAIFunctionPayload<'a> {
    name: Cow<'a, str>,
    arguments: Cow<'a, str>,
}

#[derive(Serialize, Debug)]
struct OpenAIFunctionCall<'a> {
    id: Cow<'a, str>,
    #[serde(rename = "type")]
    content_type: Cow<'a, str>,
    function: OpenAIFunctionPayload<'a>,
}

#[derive(Serialize, Debug)]
struct MessageContent<'a> {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    message_type: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<ImageUrlContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    tool_call_id: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "content")]
    tool_output: Option<Cow<'a, str>>,
}

/// Individual image message in an OpenAI chat conversation.
#[derive(Serialize, Debug)]
struct ImageUrlContent<'a> {
    url: Cow<'a, str>,
}

#[derive(Serialize)]
struct OpenAIEmbeddingRequest {
    model: String,
    input: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<u32>,
}

/// Request payload for OpenAI's chat API endpoint.
#[derive(Serialize, Debug)]
struct OpenAIChatRequest<'a> {
    model: &'a str,
    messages: Vec<OpenAIChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenAIResponseFormat>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    extra_body: Option<Map<String, Value>>,
}

/// Ordered input item for the Responses API `input` array.
///
/// The Responses protocol does not use a single `messages` array; instead it
/// replays typed items in chronological order. This mirrors that ordering so
/// reasoning/message/function items are not flattened. Full ordered replay and
/// unknown-item handling are completed in later tasks.
#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum OpenAIResponsesInputItem<'a> {
    #[serde(rename = "message")]
    Message {
        role: Cow<'a, str>,
        content: Vec<OpenAIResponsesInputContent<'a>>,
    },
    /// Continuation-safe reasoning replay: summaries stay visible while the
    /// encrypted payload is forwarded verbatim to a compatible origin.
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<Cow<'a, str>>,
        // The Responses API requires `summary` on every reasoning input item;
        // an empty array is valid but the key must always be present.
        summary: Vec<OpenAIResponsesReasoningSummary<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<Cow<'a, str>>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: Cow<'a, str>,
        name: Cow<'a, str>,
        arguments: Cow<'a, str>,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: Cow<'a, str>,
        output: OpenAIResponsesFunctionOutput<'a>,
    },
}

/// A function output is either plain text or ordered rich parts.
#[derive(Serialize, Debug)]
#[serde(untagged)]
enum OpenAIResponsesFunctionOutput<'a> {
    Text(Cow<'a, str>),
    Parts(Vec<OpenAIResponsesToolOutputPart<'a>>),
}

/// Ordered rich function-output part. Only fields valid for the selected
/// endpoint and content position are emitted.
#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum OpenAIResponsesToolOutputPart<'a> {
    #[serde(rename = "output_text")]
    OutputText { text: Cow<'a, str> },
    #[serde(rename = "input_image")]
    InputImage {
        image_url: Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<Cow<'a, str>>,
    },
    #[serde(rename = "input_file")]
    InputFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<Cow<'a, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_data: Option<Cow<'a, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_url: Option<Cow<'a, str>>,
    },
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
enum OpenAIResponsesReasoningSummaryKind {
    SummaryText,
}

#[derive(Serialize, Debug)]
struct OpenAIResponsesReasoningSummary<'a> {
    #[serde(rename = "type")]
    summary_type: OpenAIResponsesReasoningSummaryKind,
    text: Cow<'a, str>,
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum OpenAIResponsesInputContent<'a> {
    /// User-turn text: `{ "type": "input_text", "text": "..." }`
    #[serde(rename = "input_text")]
    InputText { text: Cow<'a, str> },
    /// Assistant-turn text: `{ "type": "output_text", "text": "..." }`
    #[serde(rename = "output_text")]
    OutputText { text: Cow<'a, str> },
    /// Inline image: `{ "type": "input_image", "image_url": "data:...;base64,..." }`
    #[serde(rename = "input_image")]
    InputImage {
        image_url: Cow<'a, str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<Cow<'a, str>>,
    },
    /// Inline or referenced file: only fields valid for the position are sent.
    #[serde(rename = "input_file")]
    InputFile {
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<Cow<'a, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_data: Option<Cow<'a, str>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        file_url: Option<Cow<'a, str>>,
    },
}

/// Flattened function definition used by the Responses API.
#[derive(Serialize, Debug)]
struct OpenAIResponsesTool<'a> {
    #[serde(rename = "type")]
    tool_type: &'a str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
    /// Responses sends explicit strictness. QueryMT's existing intent is
    /// non-strict when unset, so omission serializes as `false`.
    strict: bool,
}

/// Request payload for OpenAI's Responses API endpoint (`POST /responses`).
#[derive(Serialize, Debug)]
struct OpenAIResponsesRequest<'a> {
    model: &'a str,
    input: Vec<OpenAIResponsesInputItem<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    /// Responses mode is stateless locally; remote storage stays disabled.
    store: bool,
    /// Requested provider outputs, e.g. encrypted reasoning for local replay.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    /// Structured output mapping (`text.format`) for the Responses protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<OpenAIResponsesText>,
    /// Reasoning controls (`reasoning.effort`).
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAIResponsesReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAIResponsesTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    extra_body: Option<Map<String, Value>>,
}

/// Responses structured-output container: `"text": { "format": {...} }`.
#[derive(Serialize, Debug)]
struct OpenAIResponsesText {
    format: OpenAIResponsesTextFormat,
}

/// Responses `text.format` payload for JSON-schema structured output.
#[derive(Serialize, Debug)]
struct OpenAIResponsesTextFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    strict: Option<bool>,
}

/// Responses `reasoning` controls.
#[derive(Serialize, Debug)]
struct OpenAIResponsesReasoning {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<&'static str>,
}

pub struct DisplayableToolCall(pub ToolCall);
impl std::fmt::Display for DisplayableToolCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{\n  \"id\": \"{}\",\n  \"type\": \"{}\",\n  \"function\": {}\n}}",
            self.0.id,
            self.0.call_type,
            DisplayableFunctionCall(self.0.function.clone())
        )
    }
}

pub struct DisplayableFunctionCall(pub FunctionCall);
impl std::fmt::Display for DisplayableFunctionCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{{\n  \"name\": \"{}\",\n  \"arguments\": {}\n}}",
            self.0.name, self.0.arguments
        )
    }
}

/// Raw usage response from OpenAI's API, before normalization.
#[derive(Deserialize, Debug, Clone)]
struct OpenAIRawUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<OpenAIPromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<OpenAICompletionTokensDetails>,
    /// DeepSeek: flat top-level cache fields (OpenAI puts them in prompt_tokens_details)
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    prompt_cache_miss_tokens: Option<u32>,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct OpenAIPromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Deserialize, Debug, Clone, Default)]
struct OpenAICompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

impl OpenAIRawUsage {
    fn into_usage(self) -> Usage {
        // OpenAI/Anthropic: cached_tokens nested in prompt_tokens_details
        let openai_cached = self
            .prompt_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        // DeepSeek: prompt_cache_hit_tokens at top level of usage
        let deepseek_cached = self.prompt_cache_hit_tokens.unwrap_or(0);
        let cache_read = openai_cached.max(deepseek_cached);

        let reasoning = self
            .completion_tokens_details
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);
        Usage {
            input_tokens: self.prompt_tokens.saturating_sub(cache_read),
            output_tokens: self.completion_tokens.saturating_sub(reasoning),
            reasoning_tokens: reasoning,
            cache_read,
            cache_write: 0,
        }
    }
}

/// Response from OpenAI's chat API endpoint.
#[derive(Deserialize, Debug)]
struct OpenAIChatResponse {
    choices: Vec<OpenAIChatChoice>,
    usage: Option<OpenAIRawUsage>,
}

/// Raw usage object from the Responses API, before normalization.
///
/// Responses reports `input_tokens`/`output_tokens` with cached and reasoning
/// counts nested under detail objects. QueryMT's categories are exclusive, so
/// these are subtracted rather than summed.
#[derive(Deserialize, Debug, Clone)]
struct OpenAIResponsesRawUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<OpenAIPromptTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<OpenAICompletionTokensDetails>,
}

impl OpenAIResponsesRawUsage {
    fn into_usage(self) -> Usage {
        let cache_read = self
            .input_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        let reasoning = self
            .output_tokens_details
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);
        Usage {
            input_tokens: self.input_tokens.saturating_sub(cache_read),
            output_tokens: self.output_tokens.saturating_sub(reasoning),
            reasoning_tokens: reasoning,
            cache_read,
            cache_write: 0,
        }
    }
}

/// A non-streaming response from the Responses API (`POST /responses`).
#[derive(Deserialize, Debug)]
struct OpenAIResponsesResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    usage: Option<OpenAIResponsesRawUsage>,
    /// Terminal cause for incomplete responses, e.g. `max_output_tokens`.
    #[serde(default)]
    incomplete_details: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

/// Raw Responses output item. Unknown item types are retained as opaque data,
/// and unrecognized sibling fields on recognized items are captured through the
/// complete deserialized object.
#[derive(Deserialize, Debug)]
struct OpenAIResponsesOutputItem {
    #[serde(rename = "type")]
    item_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    summary: Option<Vec<OpenAIResponsesReasoningSummaryText>>,
    #[serde(default)]
    content: Option<Vec<OpenAIResponsesOutputContent>>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    phase: Option<String>,
    #[serde(default)]
    status: Option<String>,
    /// Unrecognized sibling fields on a known item type, preserved verbatim.
    #[serde(default, flatten)]
    extra: Map<String, Value>,
}

#[derive(Deserialize, Debug)]
struct OpenAIResponsesReasoningSummaryText {
    #[serde(default)]
    text: Option<String>,
    /// Unrecognized sibling fields preserved verbatim.
    #[serde(default, flatten)]
    extra: Map<String, Value>,
}

#[derive(Deserialize, Debug)]
struct OpenAIResponsesOutputContent {
    #[serde(rename = "type")]
    content_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    #[serde(default)]
    annotations: Vec<Value>,
    /// Complete payload retained, so unknown content kinds keep their full
    /// object (not only a literal `raw` field) and known kinds keep unknown
    /// siblings.
    #[serde(default, flatten)]
    extra: Map<String, Value>,
}

/// Individual choice within an OpenAI chat API response.
#[derive(Deserialize, Debug)]
struct OpenAIChatChoice {
    finish_reason: String,
    message: OpenAIChatMsg,
}

/// Message content within an OpenAI chat API response.
#[derive(Deserialize, Debug)]
struct OpenAIChatMsg {
    #[allow(dead_code)]
    role: String,
    content: Option<String>,
    #[serde(default, alias = "reasoning", alias = "reasoning_content")]
    thinking: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Deserialize, Debug)]
struct OpenAIEmbeddingData {
    embedding: Vec<f32>,
}
#[derive(Deserialize, Debug)]
struct OpenAIEmbeddingResponse {
    data: Vec<OpenAIEmbeddingData>,
}

/// An object specifying the format that the model must output.
///Setting to `{ "type": "json_schema", "json_schema": {...} }` enables Structured Outputs which ensures the model will match your supplied JSON schema. Learn more in the [Structured Outputs guide](https://platform.openai.com/docs/guides/structured-outputs).
/// Setting to `{ "type": "json_object" }` enables the older JSON mode, which ensures the message the model generates is valid JSON. Using `json_schema` is preferred for models that support it.
#[derive(Deserialize, Debug, Serialize)]
enum OpenAIResponseType {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "json_schema")]
    JsonSchema,
    #[serde(rename = "json_object")]
    JsonObject,
}

#[derive(Deserialize, Debug, Serialize)]
struct OpenAIResponseFormat {
    #[serde(rename = "type")]
    response_type: OpenAIResponseType,
    #[serde(skip_serializing_if = "Option::is_none")]
    json_schema: Option<StructuredOutputFormat>,
}

impl From<StructuredOutputFormat> for OpenAIResponseFormat {
    /// Modify the schema to ensure that it meets OpenAI's requirements.
    fn from(structured_response_format: StructuredOutputFormat) -> Self {
        // It's possible to pass a StructuredOutputJsonSchema without an actual schema.
        // In this case, just pass the StructuredOutputJsonSchema object without modifying it.
        match structured_response_format.schema {
            None => OpenAIResponseFormat {
                response_type: OpenAIResponseType::JsonSchema,
                json_schema: Some(structured_response_format),
            },
            Some(mut schema) => {
                // Although [OpenAI's specifications](https://platform.openai.com/docs/guides/structured-outputs?api-mode=chat#additionalproperties-false-must-always-be-set-in-objects) say that the "additionalProperties" field is required, my testing shows that it is not.
                // Just to be safe, add it to the schema if it is missing.
                schema = if schema.get("additionalProperties").is_none() {
                    schema["additionalProperties"] = serde_json::json!(false);
                    schema
                } else {
                    schema
                };

                OpenAIResponseFormat {
                    response_type: OpenAIResponseType::JsonSchema,
                    json_schema: Some(StructuredOutputFormat {
                        name: structured_response_format.name,
                        description: structured_response_format.description,
                        schema: Some(schema),
                        strict: structured_response_format.strict,
                    }),
                }
            }
        }
    }
}

impl From<OpenAIChatResponse> for ChatOutput {
    fn from(response: OpenAIChatResponse) -> Self {
        let text = response
            .choices
            .first()
            .and_then(|c| c.message.content.clone());
        let tool_calls = response
            .choices
            .first()
            .and_then(|c| c.message.tool_calls.clone());
        let thinking = response
            .choices
            .first()
            .and_then(|c| c.message.thinking.clone());
        let usage = response.usage.clone().map(|u| u.into_usage());
        let finish_reason = response
            .choices
            .first()
            .map(|c| match c.finish_reason.as_str() {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "content_filter" => FinishReason::ContentFilter,
                "tool_calls" | "function_call" => FinishReason::ToolCalls,
                _ => FinishReason::Unknown,
            });

        ChatOutput::from_projections(thinking, text, tool_calls, usage, finish_reason)
    }
}

impl std::fmt::Display for OpenAIChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (
            &self.choices.first().unwrap().message.content,
            &self.choices.first().unwrap().message.tool_calls,
        ) {
            (Some(content), Some(tool_calls)) => {
                for tool_call in tool_calls {
                    write!(f, "{}", DisplayableToolCall(tool_call.clone()))?;
                }
                write!(f, "{}", content)
            }
            (Some(content), None) => write!(f, "{}", content),
            (None, Some(tool_calls)) => {
                for tool_call in tool_calls {
                    write!(f, "{}", DisplayableToolCall(tool_call.clone()))?;
                }
                Ok(())
            }
            (None, None) => write!(f, ""),
        }
    }
}

pub trait OpenAIProviderConfig {
    fn api_key(&self) -> &str;
    fn auth_type(&self) -> Option<&AuthType> {
        None
    }
    fn base_url(&self) -> &Url;
    fn model(&self) -> &str;
    fn max_tokens(&self) -> Option<&u32>;
    fn temperature(&self) -> Option<&f32>;
    fn system(&self) -> &[String];
    fn timeout_seconds(&self) -> Option<&u64>;
    fn stream(&self) -> Option<&bool>;
    fn top_p(&self) -> Option<&f32>;
    fn top_k(&self) -> Option<&u32>;
    fn tools(&self) -> Option<&[Tool]>;
    fn tool_choice(&self) -> Option<&ToolChoice>;
    fn embedding_encoding_format(&self) -> Option<&str>;
    fn embedding_dimensions(&self) -> Option<&u32>;
    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        None
    }
    /// Whether to serialize prior thinking into assistant messages as
    /// `reasoning_content`.
    ///
    /// Some OpenAI-compatible APIs (DeepSeek, Kimi) require this when thinking
    /// is enabled. Others (e.g. Groq) reject the field entirely.
    fn include_reasoning_content(&self) -> bool {
        true
    }
    fn json_schema(&self) -> Option<&StructuredOutputFormat>;

    /// Selected API protocol. Defaults to Chat Completions.
    fn api_mode(&self) -> crate::ApiMode {
        crate::ApiMode::ChatCompletions
    }

    fn extra_body(&self) -> Option<Map<String, Value>> {
        None
    }

    /// Provider identity used to authorize replay of origin-scoped continuation.
    fn responses_provider(&self) -> &str {
        "openai"
    }

    /// Protocol identity used to authorize replay of origin-scoped continuation.
    fn responses_protocol(&self) -> &str {
        "responses"
    }

    /// Normalized Responses endpoint recorded in output provenance.
    ///
    /// Derived from the provider's own base URL so provenance reflects where
    /// the turn actually came from. An empty value never authorizes native
    /// replay, so a provider that does not override this stays portable-only.
    fn responses_endpoint(&self) -> String {
        self.base_url()
            .join("responses")
            .map(|url| url.to_string())
            .unwrap_or_default()
    }
}

#[derive(Deserialize, Debug)]
struct OpenAISttJsonResponse {
    text: String,
}

#[derive(Serialize, Debug)]
struct OpenAITtsRequestBody<'a> {
    model: &'a str,
    #[serde(rename = "input")]
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<&'a str>,
    #[serde(rename = "response_format", skip_serializing_if = "Option::is_none")]
    format: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    speed: Option<f32>,
}

// TODO: Move outside and make shared with others?
struct MultipartForm {
    boundary: &'static str,
    body: Vec<u8>,
}

impl MultipartForm {
    fn new(boundary: &'static str) -> Self {
        Self {
            boundary,
            body: Vec::new(),
        }
    }

    fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.boundary)
    }

    fn write_str(&mut self, s: &str) {
        self.body.extend_from_slice(s.as_bytes());
    }

    fn validate_token(s: &str) -> Result<(), LLMError> {
        if s.contains('\r') || s.contains('\n') {
            return Err(LLMError::InvalidRequest(
                "multipart field contains invalid characters".into(),
            ));
        }
        Ok(())
    }

    fn validate_filename(s: &str) -> Result<(), LLMError> {
        Self::validate_token(s)?;
        if s.contains('"') {
            return Err(LLMError::InvalidRequest(
                "multipart filename contains invalid characters".into(),
            ));
        }
        Ok(())
    }

    fn begin_part(&mut self) {
        self.write_str("--");
        self.write_str(self.boundary);
        self.write_str("\r\n");
    }

    fn text(&mut self, name: &str, value: &str) -> Result<(), LLMError> {
        Self::validate_token(name)?;

        self.begin_part();
        self.write_str("Content-Disposition: form-data; name=\"");
        self.write_str(name);
        self.write_str("\"\r\n\r\n");
        self.write_str(value);
        self.write_str("\r\n");
        Ok(())
    }

    fn file(
        &mut self,
        field_name: &str,
        filename: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<(), LLMError> {
        Self::validate_token(field_name)?;
        Self::validate_filename(filename)?;
        Self::validate_token(mime_type)?;

        self.begin_part();
        self.write_str(&format!(
            "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
            field_name, filename
        ));
        self.write_str(&format!("Content-Type: {}\r\n\r\n", mime_type));
        self.body.extend_from_slice(bytes);
        self.write_str("\r\n");
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        self.write_str("--");
        self.write_str(self.boundary);
        self.write_str("--\r\n");
        self.body
    }
}

pub fn openai_stt_request<C: OpenAIProviderConfig>(
    cfg: &C,
    req: &SttRequest,
) -> Result<Request<Vec<u8>>, LLMError> {
    let token = cfg.api_key();
    let auth = determine_effective_auth(token, cfg.auth_type(), cfg.base_url())?;

    let url = cfg
        .base_url()
        .join("audio/transcriptions")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;

    let model = req.model.as_deref().unwrap_or(cfg.model());
    let filename = req.filename.as_deref().unwrap_or("audio.wav");
    let mime_type = req.mime_type.as_deref().unwrap_or("audio/wav");

    // NOTE: Deterministic boundary to avoid randomness requirements in WASM.
    let boundary = "qmt-stt-boundary-7MA4YWxkTrZu0gW";

    let mut form = MultipartForm::new(boundary);
    form.text("model", model)?;
    form.text("response_format", "json")?;
    if let Some(language) = req.language.as_deref() {
        form.text("language", language)?;
    }
    form.file("file", filename, mime_type, &req.audio)?;
    let content_type = form.content_type();
    let body = form.finish();

    let builder = Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(CONTENT_TYPE, content_type);
    let builder = maybe_add_auth_header(builder, &auth, token)?;
    Ok(builder.body(body)?)
}

pub fn openai_parse_stt<C: OpenAIProviderConfig>(
    _cfg: &C,
    resp: Response<Vec<u8>>,
) -> Result<SttResponse, LLMError> {
    handle_http_error!(resp);

    if let Ok(json_resp) = serde_json::from_slice::<OpenAISttJsonResponse>(resp.body()) {
        return Ok(SttResponse {
            text: json_resp.text,
        });
    }

    let text = String::from_utf8(resp.body().to_vec())?;
    Ok(SttResponse { text })
}

pub fn openai_tts_request<C: OpenAIProviderConfig>(
    cfg: &C,
    req: &TtsRequest,
) -> Result<Request<Vec<u8>>, LLMError> {
    use querymt::tts::VoiceConfig;

    // OpenAI only supports named preset voices.
    let voice: Option<&str> = match &req.voice_config {
        Some(VoiceConfig::Preset { name }) => Some(name.as_str()),
        Some(VoiceConfig::Clone { .. }) => {
            return Err(LLMError::NotImplemented(
                "voice cloning is not supported by the OpenAI TTS API".into(),
            ));
        }
        Some(VoiceConfig::Design { .. }) => {
            return Err(LLMError::NotImplemented(
                "voice design is not supported by the OpenAI TTS API".into(),
            ));
        }
        None => None,
    };

    let token = cfg.api_key();
    let auth = determine_effective_auth(token, cfg.auth_type(), cfg.base_url())?;

    let url = cfg
        .base_url()
        .join("audio/speech")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;

    let model = req.model.as_deref().unwrap_or(cfg.model());

    let body = OpenAITtsRequestBody {
        model,
        text: &req.text,
        voice,
        format: req.format.as_deref(),
        speed: req.speed,
    };
    let json_body = serde_json::to_vec(&body)?;

    let builder = Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(CONTENT_TYPE, "application/json");
    let builder = maybe_add_auth_header(builder, &auth, token)?;
    Ok(builder.body(json_body)?)
}

pub fn openai_parse_tts<C: OpenAIProviderConfig>(
    _cfg: &C,
    resp: Response<Vec<u8>>,
) -> Result<TtsResponse, LLMError> {
    handle_http_error!(resp);

    let mime_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    Ok(TtsResponse {
        audio: resp.body().clone(),
        mime_type,
    })
}

fn is_openai_host(base_url: &Url) -> bool {
    matches!(base_url.host_str(), Some("api.openai.com"))
}

fn token_hint(token: &str) -> String {
    let len = token.chars().count();
    if len <= 10 {
        return "<redacted>".to_string();
    }
    let prefix: String = token.chars().take(6).collect();
    let suffix: String = token
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}...{suffix}")
}

fn determine_auth_type(token: &str, explicit: Option<&AuthType>, base_url: &Url) -> AuthType {
    if !is_openai_host(base_url) {
        return AuthType::ApiKey;
    }

    if let Some(auth_type) = explicit {
        return auth_type.clone();
    }

    if token.starts_with("sk-") {
        return AuthType::ApiKey;
    }

    let dot_segments = token.split('.').count();
    if dot_segments == 3 || token.starts_with("eyJ") {
        return AuthType::OAuth;
    }

    eprintln!(
        "Warning: OpenAI token format not recognized (expected 'sk-' or JWT). \
        Defaulting to API key authentication. Consider setting 'auth_type' explicitly."
    );
    AuthType::ApiKey
}

fn determine_effective_auth(
    token: &str,
    explicit: Option<&AuthType>,
    base_url: &Url,
) -> Result<AuthType, LLMError> {
    // Allow explicitly disabling auth for non-OpenAI hosts.
    if matches!(explicit, Some(AuthType::NoAuth)) {
        if is_openai_host(base_url) {
            return Err(LLMError::AuthError(
                "OpenAI (api.openai.com) requires authentication".to_string(),
            ));
        }
        return Ok(AuthType::NoAuth);
    }

    // Official OpenAI host: always require a token.
    if is_openai_host(base_url) {
        if token.is_empty() {
            return Err(LLMError::AuthError("Missing OpenAI auth token".to_string()));
        }
        return Ok(determine_auth_type(token, explicit, base_url));
    }

    // OpenAI-compatible/self-hosted endpoints.
    if token.is_empty() {
        return Ok(AuthType::NoAuth);
    }

    if matches!(explicit, Some(AuthType::OAuth)) {
        println!(
            "Warning: OpenAI OAuth auth_type is only supported for api.openai.com; \
            using API key authentication."
        );
    }
    Ok(AuthType::ApiKey)
}

fn maybe_add_auth_header(
    mut builder: http::request::Builder,
    auth: &AuthType,
    token: &str,
) -> Result<http::request::Builder, LLMError> {
    match auth {
        AuthType::NoAuth => Ok(builder),
        _ => {
            if token.is_empty() {
                return Err(LLMError::AuthError("Missing OpenAI auth token".to_string()));
            }
            builder = builder.header(AUTHORIZATION, format!("Bearer {}", token));
            Ok(builder)
        }
    }
}

pub fn openai_embed_request<C: OpenAIProviderConfig>(
    cfg: &C,
    inputs: &[String],
) -> Result<Request<Vec<u8>>, LLMError> {
    let token = cfg.api_key();
    let auth = determine_effective_auth(token, cfg.auth_type(), cfg.base_url())?;

    let emb_format = cfg.embedding_encoding_format().unwrap_or("float");

    let body = OpenAIEmbeddingRequest {
        model: cfg.model().into(),
        input: inputs.to_vec(),
        encoding_format: Some(emb_format.into()),
        dimensions: cfg.embedding_dimensions().copied(),
    };

    let url = cfg
        .base_url()
        .join("embeddings")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;
    let json_body = serde_json::to_vec(&body).unwrap();
    let builder = Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(CONTENT_TYPE, "application/json");
    let builder = maybe_add_auth_header(builder, &auth, token)?;
    Ok(builder.body(json_body)?)
}

pub fn openai_parse_embed<C: OpenAIProviderConfig>(
    _cfg: &C,
    resp: Response<Vec<u8>>,
) -> Result<Vec<Vec<f32>>, LLMError> {
    let json_resp: OpenAIEmbeddingResponse = serde_json::from_slice(resp.body())?;
    let embeddings = json_resp.data.into_iter().map(|d| d.embedding).collect();
    Ok(embeddings)
}

/// Validate every message before request serialization.
///
/// A message carrying authoritative structured output must still agree with its
/// portable projection. If a caller, hook, or history editor changed the
/// projected content without clearing the authoritative output, sending would
/// silently reinstate the stale payload (and resend redacted content), so the
/// request fails explicitly instead.
pub fn validate_chat_messages(messages: &[ChatMessage]) -> Result<(), LLMError> {
    for message in messages {
        message.validate_output_consistency().map_err(|error| {
            LLMError::InvalidRequest(format!("inconsistent structured message: {error}"))
        })?;
    }
    Ok(())
}

fn validate_chat_completions_attachments(messages: &[ChatMessage]) -> Result<(), LLMError> {
    for message in messages {
        for part in message.portable_input_parts() {
            match part {
                ChatInputPart::Attachment(media) if media.kind != MediaKind::Image => {
                    return Err(LLMError::InvalidRequest(format!(
                        "Chat Completions does not support {:?} message attachments",
                        media.kind
                    )));
                }
                ChatInputPart::Attachment(media)
                    if matches!(media.source(), MediaSource::ProviderFile { .. }) =>
                {
                    return Err(LLMError::InvalidRequest(
                        "Chat Completions cannot replay provider-file message attachments"
                            .to_string(),
                    ));
                }
                ChatInputPart::ToolResult(result) => {
                    for result_part in &result.parts {
                        let ToolResultPart::Attachment(media) = result_part else {
                            continue;
                        };
                        if media.kind != MediaKind::Image
                            || matches!(media.source(), MediaSource::ProviderFile { .. })
                        {
                            return Err(LLMError::InvalidRequest(format!(
                                "Chat Completions does not support {:?} tool-result attachments",
                                media.kind
                            )));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

pub fn openai_chat_request<C: OpenAIProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Request<Vec<u8>>, LLMError> {
    validate_chat_messages(messages)?;
    validate_chat_completions_attachments(messages)?;
    let token = cfg.api_key();
    let auth = determine_effective_auth(token, cfg.auth_type(), cfg.base_url())?;

    let mut openai_msgs: Vec<OpenAIChatMessage<'_>> = vec![];
    let include_reasoning = cfg.include_reasoning_content();

    for msg in messages {
        convert_chat_message_to_openai(msg, &mut openai_msgs, include_reasoning);
    }

    let system_parts = cfg.system();
    if !system_parts.is_empty() {
        // Insert system messages in reverse order at position 0
        // so they end up in the correct order.
        for part in system_parts.iter().rev() {
            openai_msgs.insert(
                0,
                OpenAIChatMessage {
                    role: Cow::Borrowed("system"),
                    content: Some(Left(vec![MessageContent {
                        message_type: Some(Cow::Borrowed("text")),
                        text: Some(Cow::Borrowed(part)),
                        image_url: None,
                        tool_call_id: None,
                        tool_output: None,
                    }])),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                },
            );
        }
    }

    // Build the response format object
    let response_format: Option<OpenAIResponseFormat> = cfg.json_schema().cloned().map(Into::into);

    let request_tools = tools
        .map(|t| t.to_vec())
        .or_else(|| cfg.tools().map(|t| t.to_vec()));

    let request_tool_choice = if request_tools.is_some() {
        cfg.tool_choice().cloned()
    } else {
        None
    };

    let extra_body = cfg.extra_body().map(|m| {
        if should_snakecase_extra_body(cfg.base_url()) {
            normalize_extra_body_map(m)
        } else {
            m
        }
    });

    let body = OpenAIChatRequest {
        model: cfg.model(),
        messages: openai_msgs,
        max_tokens: cfg.max_tokens().copied(),
        temperature: cfg.temperature().copied(),
        stream: *cfg.stream().unwrap_or(&false),
        top_p: cfg.top_p().copied(),
        top_k: cfg.top_k().copied(),
        tools: request_tools,
        tool_choice: request_tool_choice,
        reasoning_effort: cfg
            .reasoning_effort()
            .map(|e| openai_effort_str(e).to_owned()),
        response_format,
        extra_body,
    };

    let json_body = serde_json::to_vec(&body)?;
    let url = cfg
        .base_url()
        .join("chat/completions")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;

    let builder = Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(CONTENT_TYPE, "application/json");
    let builder = maybe_add_auth_header(builder, &auth, token)?;
    Ok(builder.body(json_body)?)
}

/// Convert history into ordered Responses `input` items.
///
/// Unlike Chat Completions, the Responses API replays typed items in
/// chronological order rather than a single `messages` array. When a turn
/// carries authoritative structured output, that output is replayed and the
/// portable `content` projection is ignored to avoid duplicate items.
///
/// Replay is input-valid: reasoning, message text, and function calls/outputs
/// have validated Responses representations. An item that is required for
/// continuation but has no validated input representation (an opaque provider
/// item) fails explicitly instead of being silently dropped.
fn convert_chat_messages_to_responses<'a>(
    messages: &'a [ChatMessage],
    out: &mut Vec<OpenAIResponsesInputItem<'a>>,
    target: &ChatOutputProvenance,
) -> Result<(), LLMError> {
    for msg in messages {
        if msg.role == ChatRole::Assistant
            && let Some(output) = msg.output()
        {
            let native_replay = output.provenance.as_ref() == Some(target);
            convert_structured_output_to_responses(output, out, native_replay)?;
            continue;
        }

        let is_user = matches!(msg.role, ChatRole::User);
        let role = Cow::Borrowed(if is_user { "user" } else { "assistant" });
        let mut content: Vec<OpenAIResponsesInputContent<'a>> = Vec::new();
        for part in msg.input().into_iter().flatten() {
            match part {
                ChatInputPart::Text { text } if !text.is_empty() => {
                    content.push(if is_user {
                        OpenAIResponsesInputContent::InputText {
                            text: Cow::Borrowed(text),
                        }
                    } else {
                        OpenAIResponsesInputContent::OutputText {
                            text: Cow::Borrowed(text),
                        }
                    });
                }
                ChatInputPart::Text { .. } => {}
                ChatInputPart::Attachment(media) => match media.source() {
                    MediaSource::Inline { data } => {
                        let content_type = media
                            .media_type()
                            .map(ToString::to_string)
                            .unwrap_or_default();
                        if media.kind == MediaKind::Image {
                            content.push(OpenAIResponsesInputContent::InputImage {
                                image_url: Cow::Owned(format!(
                                    "data:{};base64,{}",
                                    content_type,
                                    base64::engine::general_purpose::STANDARD.encode(data)
                                )),
                                detail: media.detail.as_deref().map(Cow::Borrowed),
                            });
                        } else if media.kind == MediaKind::Document {
                            content.push(OpenAIResponsesInputContent::InputFile {
                                filename: media.filename.as_deref().map(Cow::Borrowed),
                                file_data: Some(Cow::Owned(format!(
                                    "data:{};base64,{}",
                                    content_type,
                                    base64::engine::general_purpose::STANDARD.encode(data)
                                ))),
                                file_url: None,
                            });
                        } else {
                            return Err(LLMError::InvalidRequest(format!(
                                "unsupported Responses input attachment kind: {:?}",
                                media.kind
                            )));
                        }
                    }
                    MediaSource::DataUrl { url } | MediaSource::Url { url } => {
                        if media.kind == MediaKind::Image {
                            content.push(OpenAIResponsesInputContent::InputImage {
                                image_url: Cow::Borrowed(url),
                                detail: media.detail.as_deref().map(Cow::Borrowed),
                            });
                        } else if media.kind == MediaKind::Document {
                            content.push(OpenAIResponsesInputContent::InputFile {
                                filename: media.filename.as_deref().map(Cow::Borrowed),
                                file_data: None,
                                file_url: Some(Cow::Borrowed(url)),
                            });
                        } else {
                            return Err(LLMError::InvalidRequest(format!(
                                "unsupported Responses input attachment kind: {:?}",
                                media.kind
                            )));
                        }
                    }
                    MediaSource::ProviderFile { .. } => {
                        return Err(LLMError::InvalidRequest(
                            "provider file references are origin-scoped and cannot be replayed as portable Responses input".to_string(),
                        ));
                    }
                },
                ChatInputPart::ToolResult(result) => {
                    flush_responses_message(out, role.clone(), &mut content);
                    let output = responses_function_output_parts(&result.parts)?;
                    out.push(OpenAIResponsesInputItem::FunctionCallOutput {
                        call_id: Cow::Borrowed(&result.call_id),
                        output,
                    });
                }
            }
        }

        flush_responses_message(out, role, &mut content);
    }

    Ok(())
}

/// Replay authoritative structured output as validated ordered input items.
fn convert_structured_output_to_responses<'a>(
    output: &'a ChatOutput,
    out: &mut Vec<OpenAIResponsesInputItem<'a>>,
    native_replay: bool,
) -> Result<(), LLMError> {
    for item in &output.items {
        match item {
            ChatOutputItem::Reasoning(reasoning) => {
                let has_native_state = reasoning.id.is_some()
                    || reasoning.encrypted_content.is_some()
                    || reasoning.signature.is_some()
                    || !reasoning.extensions.is_empty();
                if has_native_state && !native_replay {
                    let visible = reasoning.visible_text();
                    if !visible.is_empty() {
                        out.push(OpenAIResponsesInputItem::Message {
                            role: Cow::Borrowed("assistant"),
                            content: vec![OpenAIResponsesInputContent::OutputText {
                                text: Cow::Owned(visible),
                            }],
                        });
                    }
                    continue;
                }
                out.push(OpenAIResponsesInputItem::Reasoning {
                    id: reasoning.id.as_deref().map(Cow::Borrowed),
                    summary: reasoning
                        .summary
                        .iter()
                        .chain(&reasoning.content)
                        .filter(|part| !part.text.is_empty())
                        .map(|part| OpenAIResponsesReasoningSummary {
                            summary_type: OpenAIResponsesReasoningSummaryKind::SummaryText,
                            text: Cow::Borrowed(part.text.as_str()),
                        })
                        .collect(),
                    encrypted_content: reasoning.encrypted_content.as_deref().map(Cow::Borrowed),
                });
            }
            ChatOutputItem::Message(message) => {
                let mut content: Vec<OpenAIResponsesInputContent<'a>> = Vec::new();
                for part in &message.parts {
                    match part {
                        ChatMessagePart::Text { text, .. } if !text.is_empty() => {
                            content.push(OpenAIResponsesInputContent::OutputText {
                                text: Cow::Borrowed(text.as_str()),
                            });
                        }
                        ChatMessagePart::Refusal { refusal, .. } if !refusal.is_empty() => {
                            content.push(OpenAIResponsesInputContent::OutputText {
                                text: Cow::Borrowed(refusal.as_str()),
                            });
                        }
                        ChatMessagePart::Media(media) => {
                            let emitted = responses_message_media(media)?.ok_or_else(|| {
                                LLMError::InvalidRequest(
                                    "Responses continuation contains media with no validated input representation for this endpoint".to_string(),
                                )
                            })?;
                            content.push(emitted);
                        }
                        ChatMessagePart::Opaque(opaque) if native_replay => {
                            return Err(LLMError::InvalidRequest(format!(
                                "unsupported Responses continuation: message part type '{}' has no validated input representation",
                                opaque.original_type
                            )));
                        }
                        ChatMessagePart::Opaque(_)
                        | ChatMessagePart::Text { .. }
                        | ChatMessagePart::Refusal { .. } => {}
                    }
                }
                if !content.is_empty() {
                    out.push(OpenAIResponsesInputItem::Message {
                        role: Cow::Borrowed("assistant"),
                        content,
                    });
                }
            }
            ChatOutputItem::FunctionCall(call) => {
                out.push(OpenAIResponsesInputItem::FunctionCall {
                    call_id: Cow::Borrowed(call.call_id.as_str()),
                    name: Cow::Borrowed(call.name.as_str()),
                    arguments: Cow::Borrowed(call.arguments.as_str()),
                });
            }
            ChatOutputItem::Opaque(opaque) => {
                return Err(LLMError::InvalidRequest(format!(
                    "unsupported Responses continuation: output item type '{}' has no validated input representation",
                    opaque.original_type
                )));
            }
        }
    }

    Ok(())
}

fn flush_responses_message<'a>(
    out: &mut Vec<OpenAIResponsesInputItem<'a>>,
    role: Cow<'a, str>,
    content: &mut Vec<OpenAIResponsesInputContent<'a>>,
) {
    if content.is_empty() {
        return;
    }
    out.push(OpenAIResponsesInputItem::Message {
        role,
        content: std::mem::take(content),
    });
}

/// Convert one typed media part into a Responses message content part.
///
/// Consumes validated `MediaType` values (never re-parses raw strings) and
/// preserves the source form:
/// - inline bytes -> protocol-valid `input_image`/`input_file` with `data:` payloads
/// - ordinary/data URLs -> URL-bearing image/file fields
/// - provider file references -> only when their recorded origin matches the
///   target; a cross-origin reference is rejected rather than forwarded as if
///   it were a portable URL.
///
/// Media kinds this endpoint cannot represent return `None` (they remain in
/// canonical history and are handled by the display/attachment contract) while
/// an unsupported *required* continuation is surfaced as an explicit error.
fn responses_message_media<'a>(
    media: &'a querymt::chat::MediaPart,
) -> Result<Option<OpenAIResponsesInputContent<'a>>, LLMError> {
    use querymt::chat::{MediaKind, MediaSource};

    match media.source() {
        MediaSource::Inline { data } => {
            let Some(media_type) = media.media_type() else {
                // Inline media always carries a validated MIME type by contract.
                return Err(LLMError::InvalidRequest(
                    "inline Responses media is missing its validated MIME type".to_string(),
                ));
            };
            let data_url = encode_data_url(media_type, data);
            match media.kind {
                MediaKind::Image => Ok(Some(OpenAIResponsesInputContent::InputImage {
                    image_url: Cow::Owned(data_url),
                    detail: media.detail.clone().map(Cow::Owned),
                })),
                MediaKind::Document | MediaKind::Other => {
                    Ok(Some(OpenAIResponsesInputContent::InputFile {
                        filename: media.filename.clone().map(Cow::Owned),
                        file_data: Some(Cow::Owned(data_url)),
                        file_url: None,
                    }))
                }
                // Audio/video have no message-position representation here.
                MediaKind::Audio | MediaKind::Video => Ok(None),
            }
        }
        MediaSource::DataUrl { url } | MediaSource::Url { url } => match media.kind {
            MediaKind::Image => Ok(Some(OpenAIResponsesInputContent::InputImage {
                image_url: Cow::Borrowed(url.as_str()),
                detail: media.detail.clone().map(Cow::Owned),
            })),
            MediaKind::Document | MediaKind::Other => {
                Ok(Some(OpenAIResponsesInputContent::InputFile {
                    filename: media.filename.clone().map(Cow::Owned),
                    file_data: None,
                    file_url: Some(Cow::Borrowed(url.as_str())),
                }))
            }
            MediaKind::Audio | MediaKind::Video => Ok(None),
        },
        MediaSource::ProviderFile { file_id, origin } => Err(LLMError::InvalidRequest(format!(
            "provider file reference '{file_id}' from {}/{} cannot be replayed as a portable \
             Responses URL",
            origin.provider, origin.endpoint
        ))),
    }
}

/// Build a function output that preserves rich part order and correlates by
/// `call_id` (the caller supplies the id).
///
/// Text stays text when it is the only content. Image and supported file parts
/// become ordered rich parts. Media this endpoint cannot represent fails
/// explicitly instead of being replaced by a textual placeholder.
/// Serialize a canonical tool result's ordered parts into a Responses function
/// output, correlated by `call_id`.
///
/// Text stays text when it is the only content. Image and supported file parts
/// become ordered rich parts. Media this endpoint cannot represent fails
/// explicitly instead of being replaced by a textual placeholder.
fn responses_function_output_parts(
    parts: &[ToolResultPart],
) -> Result<OpenAIResponsesFunctionOutput<'static>, LLMError> {
    // Text-only results keep the compact string form.
    let is_text_only = parts
        .iter()
        .all(|part| matches!(part, ToolResultPart::Text { .. }));
    if is_text_only {
        let text = parts
            .iter()
            .filter_map(ToolResultPart::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(OpenAIResponsesFunctionOutput::Text(Cow::Owned(text)));
    }

    let mut rich: Vec<OpenAIResponsesToolOutputPart<'static>> = Vec::new();
    for part in parts {
        match part {
            ToolResultPart::Text { text } => {
                rich.push(OpenAIResponsesToolOutputPart::OutputText {
                    text: Cow::Owned(text.clone()),
                });
            }
            ToolResultPart::Attachment(media) => match (&media.kind, media.source()) {
                (MediaKind::Image, MediaSource::Inline { data }) => {
                    let media_type =
                        media.media_type().map(ToString::to_string).ok_or_else(|| {
                            LLMError::InvalidRequest(
                                "inline tool-result image requires a MIME type".to_string(),
                            )
                        })?;
                    rich.push(OpenAIResponsesToolOutputPart::InputImage {
                        image_url: Cow::Owned(format!(
                            "data:{};base64,{}",
                            media_type,
                            base64::engine::general_purpose::STANDARD.encode(data)
                        )),
                        detail: media.detail.clone().map(Cow::Owned),
                    });
                }
                (MediaKind::Image, MediaSource::DataUrl { url } | MediaSource::Url { url }) => {
                    rich.push(OpenAIResponsesToolOutputPart::InputImage {
                        image_url: Cow::Owned(url.clone()),
                        detail: media.detail.clone().map(Cow::Owned),
                    });
                }
                (MediaKind::Document, MediaSource::Inline { data }) => {
                    let media_type =
                        media.media_type().map(ToString::to_string).ok_or_else(|| {
                            LLMError::InvalidRequest(
                                "inline tool-result file requires a MIME type".to_string(),
                            )
                        })?;
                    rich.push(OpenAIResponsesToolOutputPart::InputFile {
                        filename: media.filename.clone().map(Cow::Owned),
                        file_data: Some(Cow::Owned(format!(
                            "data:{};base64,{}",
                            media_type,
                            base64::engine::general_purpose::STANDARD.encode(data)
                        ))),
                        file_url: None,
                    });
                }
                (MediaKind::Document, MediaSource::DataUrl { url } | MediaSource::Url { url }) => {
                    rich.push(OpenAIResponsesToolOutputPart::InputFile {
                        filename: media.filename.clone().map(Cow::Owned),
                        file_data: None,
                        file_url: Some(Cow::Owned(url.clone())),
                    });
                }
                (kind, source) => {
                    // Unsupported media kinds (audio/video/other) have no
                    // protocol-valid representation and must fail explicitly
                    // instead of being substituted with a placeholder file.
                    let _ = source;
                    return Err(LLMError::InvalidRequest(format!(
                        "unsupported Responses function output media: kind {kind:?} has no \
                         protocol-valid representation"
                    )));
                }
            },
        }
    }

    Ok(OpenAIResponsesFunctionOutput::Parts(rich))
}

fn to_responses_tools(tools: &[Tool]) -> Result<Vec<OpenAIResponsesTool<'_>>, LLMError> {
    tools
        .iter()
        .map(|tool| {
            // Responses requires explicit strictness; QueryMT's existing intent
            // is non-strict when unspecified.
            let strict = tool.function.strict.unwrap_or(false);
            if strict {
                validate_responses_strict_schema(&tool.function.name, &tool.function.parameters)?;
            }
            Ok(OpenAIResponsesTool {
                tool_type: tool.tool_type.as_str(),
                name: tool.function.name.as_str(),
                description: tool.function.description.as_str(),
                parameters: &tool.function.parameters,
                strict,
            })
        })
        .collect()
}

/// Validate that a schema can be sent with `strict=true` without changing its
/// meaning.
///
/// Responses strict mode requires `additionalProperties: false` on every object
/// and that every property is listed as required. Rather than silently
/// promoting formerly optional properties to required (which would change
/// caller intent), an incompatible schema is rejected.
fn validate_responses_strict_schema(name: &str, schema: &Value) -> Result<(), LLMError> {
    fn check(name: &str, schema: &Value) -> Result<(), LLMError> {
        let Some(object) = schema.as_object() else {
            return Ok(());
        };

        if object.get("type").and_then(Value::as_str) == Some("object") {
            match object.get("additionalProperties") {
                Some(Value::Bool(false)) => {}
                _ => {
                    return Err(LLMError::InvalidRequest(format!(
                        "strict schema error for tool '{name}': every object must set \
                         'additionalProperties: false' for Responses strict validation"
                    )));
                }
            }

            let properties = object
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let required: Vec<&str> = object
                .get("required")
                .and_then(Value::as_array)
                .map(|entries| entries.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if let Some(optional) = properties
                .keys()
                .find(|key| !required.contains(&key.as_str()))
            {
                return Err(LLMError::InvalidRequest(format!(
                    "strict schema error for tool '{name}': optional property '{optional}' \
                     would have to become required, changing caller intent"
                )));
            }

            for (key, value) in &properties {
                check(&format!("{name}.{key}"), value)?;
            }
        }

        // Array item schemas are validated independently of the parent type so
        // objects nested inside arrays cannot escape strict validation.
        if let Some(items) = object.get("items") {
            check(&format!("{name}[]"), items)?;
        }

        Ok(())
    }

    check(name, schema)
}

/// Protocol-correct named tool choice for the Responses API.
fn responses_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Any => Value::String("required".to_string()),
        ToolChoice::Auto => Value::String("auto".to_string()),
        ToolChoice::None => Value::String("none".to_string()),
        ToolChoice::Tool(name) => serde_json::json!({ "type": "function", "name": name }),
    }
}

/// Extra-body keys that Responses mode owns. Supplying them would silently
/// change storage or continuation policy, so construction rejects them instead
/// of letting a duplicate key or override win.
const RESPONSES_RESERVED_EXTRA_BODY_KEYS: &[&str] =
    &["store", "previous_response_id", "conversation", "include"];

/// Reject extra-body entries that conflict with Responses-owned protocol policy.
///
/// The reserved names cover the two stateful continuation mechanisms
/// (`previous_response_id`, `conversation`) and the storage/continuation policy
/// fields (`store`, `include`) so a caller cannot silently flip them via
/// passthrough configuration.
fn reject_reserved_extra_body_keys(extra_body: &Map<String, Value>) -> Result<(), LLMError> {
    for existing in extra_body.keys() {
        let normalized = existing.to_snake_case();
        if let Some(key) = RESPONSES_RESERVED_EXTRA_BODY_KEYS
            .iter()
            .find(|key| normalized == **key)
        {
            return Err(LLMError::InvalidRequest(format!(
                "conflicting Responses request configuration: '{existing}' resolves to \
                 protocol-owned field '{key}' and cannot be supplied via extra_body"
            )));
        }
    }
    Ok(())
}

/// Build a stateless `POST /responses` request.
///
/// This is the opt-in Responses path selected via [`crate::ApiMode`]. It never
/// falls back to Chat Completions, disables remote storage, requests encrypted
/// reasoning for local replay, and never emits stateful continuation
/// references. System config maps to `instructions`, `max_tokens` to
/// `max_output_tokens`, a JSON schema to `text.format`, and reasoning effort to
/// `reasoning.effort`. Unsupported controls are rejected explicitly rather than
/// being silently dropped or copied from another backend's contract.
pub fn openai_responses_request<C: OpenAIProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Request<Vec<u8>>, LLMError> {
    validate_chat_messages(messages)?;
    let token = cfg.api_key();
    let auth = determine_effective_auth(token, cfg.auth_type(), cfg.base_url())?;

    // Responses has no `top_k` equivalent. Ignoring it would silently change
    // sampling behavior, so reject it explicitly.
    if cfg.top_k().is_some() {
        return Err(LLMError::InvalidRequest(
            "unsupported parameter for Responses mode: 'top_k' has no Responses equivalent"
                .to_string(),
        ));
    }

    let target = ChatOutputProvenance {
        provider: cfg.responses_provider().to_string(),
        protocol: cfg.responses_protocol().to_string(),
        model: cfg.model().to_string(),
        endpoint: cfg.responses_endpoint(),
    };
    let mut input: Vec<OpenAIResponsesInputItem<'_>> = Vec::new();
    convert_chat_messages_to_responses(messages, &mut input, &target)?;

    let instructions = {
        let parts = cfg.system();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n\n"))
        }
    };

    let request_tools = match tools {
        Some(tools) => Some(to_responses_tools(tools)?),
        None => match cfg.tools() {
            Some(tools) => Some(to_responses_tools(tools)?),
            None => None,
        },
    };
    let request_tool_choice = if request_tools.is_some() {
        cfg.tool_choice().map(responses_tool_choice)
    } else {
        None
    };

    let extra_body = cfg.extra_body().map(|m| {
        if should_snakecase_extra_body(cfg.base_url()) {
            normalize_extra_body_map(m)
        } else {
            m
        }
    });

    if let Some(extra_body) = &extra_body {
        // Check both the normalized and original spellings so camelCase aliases
        // cannot slip through on non-OpenAI hosts (which skip normalization).
        reject_reserved_extra_body_keys(extra_body)?;
        if let Some(raw) = cfg.extra_body() {
            reject_reserved_extra_body_keys(&raw)?;
        }
    }

    let body = OpenAIResponsesRequest {
        model: cfg.model(),
        input,
        instructions: instructions.as_deref(),
        // Stateless local continuation: never store remotely and request the
        // encrypted reasoning needed to replay reasoning items locally.
        store: false,
        include: vec!["reasoning.encrypted_content"],
        max_output_tokens: cfg.max_tokens().copied(),
        temperature: cfg.temperature().copied(),
        stream: *cfg.stream().unwrap_or(&false),
        top_p: cfg.top_p().copied(),
        text: cfg
            .json_schema()
            .cloned()
            .map(|format| OpenAIResponsesText {
                format: OpenAIResponsesTextFormat {
                    format_type: "json_schema",
                    name: format.name,
                    description: format.description,
                    schema: format.schema,
                    strict: format.strict,
                },
            }),
        reasoning: cfg
            .reasoning_effort()
            .map(|effort| OpenAIResponsesReasoning {
                effort: Some(openai_effort_str(effort)),
            }),
        tools: request_tools,
        tool_choice: request_tool_choice,
        extra_body,
    };

    let json_body = serde_json::to_vec(&body)?;
    let url = cfg
        .base_url()
        .join("responses")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;

    let builder = Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(CONTENT_TYPE, "application/json");
    let builder = maybe_add_auth_header(builder, &auth, token)?;
    Ok(builder.body(json_body)?)
}

pub fn openai_parse_chat<C: OpenAIProviderConfig>(
    cfg: &C,
    response: Response<Vec<u8>>,
) -> Result<ChatOutput, LLMError> {
    openai_parse_chat_with(cfg, response, None)
}

pub fn openai_parse_chat_with<C: OpenAIProviderConfig>(
    _cfg: &C,
    response: Response<Vec<u8>>,
    provider_classifier: Option<OpenAIErrorClassifier>,
) -> Result<ChatOutput, LLMError> {
    if !response.status().is_success() {
        return Err(classify_openai_http_error_with(
            &response,
            provider_classifier,
        ));
    }

    let envelope: Value =
        serde_json::from_slice(response.body()).map_err(|error| LLMError::ResponseFormatError {
            message: format!("Failed to decode API response: {error}"),
            raw_response: String::from_utf8_lossy(response.body()).into_owned(),
        })?;
    if let Some(error) = envelope.get("error") {
        let explicit_request_id = envelope
            .get("request_id")
            .or_else(|| envelope.get("requestId"))
            .or_else(|| envelope.get("id"))
            .and_then(Value::as_str);
        return Err(map_openai_error_envelope(
            error,
            &envelope,
            explicit_request_id,
            false,
            provider_classifier,
        )
        .into());
    }

    serde_json::from_value::<OpenAIChatResponse>(envelope)
        .map(ChatOutput::from)
        .map_err(|error| LLMError::ResponseFormatError {
            message: format!("Failed to decode API response: {error}"),
            raw_response: String::from_utf8_lossy(response.body()).into_owned(),
        })
}

/// Parse and normalize a non-streaming Responses API response.
///
/// Preserves ordered structured items (reasoning, message, function calls),
/// retains unsupported built-in items as opaque data without executing them,
/// preserves refusal/annotation data, and normalizes usage into exclusive
/// cached-input, ordinary-input, reasoning-output, and ordinary-output
/// categories. Incomplete/failed responses keep partial output and terminal
/// cause instead of reporting a successful stop.
pub fn openai_parse_responses<C: OpenAIProviderConfig>(
    cfg: &C,
    response: Response<Vec<u8>>,
    provider_classifier: Option<OpenAIErrorClassifier>,
) -> Result<ChatOutput, LLMError> {
    openai_parse_responses_with(
        cfg,
        response,
        provider_classifier,
        ResponsesCodecProvider::default(),
    )
}

/// Parse and normalize a non-streaming Responses API response, recording the
/// caller-supplied codec provenance (provider identity and endpoint).
pub fn openai_parse_responses_with<C: OpenAIProviderConfig>(
    cfg: &C,
    response: Response<Vec<u8>>,
    provider_classifier: Option<OpenAIErrorClassifier>,
    codec_provider: ResponsesCodecProvider,
) -> Result<ChatOutput, LLMError> {
    if !response.status().is_success() {
        return Err(classify_openai_http_error_with(
            &response,
            provider_classifier,
        ));
    }

    let envelope: Value =
        serde_json::from_slice(response.body()).map_err(|error| LLMError::ResponseFormatError {
            message: format!("Failed to decode Responses API response: {error}"),
            raw_response: String::from_utf8_lossy(response.body()).into_owned(),
        })?;

    // A top-level `error` (or a `failed` status with an error object) is a
    // provider failure with classified details.
    if let Some(error) = envelope.get("error").filter(|error| !error.is_null()) {
        let explicit_request_id = envelope.get("id").and_then(Value::as_str);
        return Err(map_openai_error_envelope(
            error,
            &envelope,
            explicit_request_id,
            false,
            provider_classifier,
        )
        .into());
    }

    let parsed: OpenAIResponsesResponse =
        serde_json::from_value(envelope).map_err(|error| LLMError::ResponseFormatError {
            message: format!("Failed to decode Responses API response: {error}"),
            raw_response: String::from_utf8_lossy(response.body()).into_owned(),
        })?;

    // Provenance reflects the endpoint the response actually came from.
    let codec_provider = ResponsesCodecProvider {
        endpoint: cfg.responses_endpoint(),
        ..codec_provider
    };
    Ok(normalize_responses_response_with(parsed, codec_provider))
}

/// Normalize a decoded Responses response into provider-neutral structured output.
/// Codec behavior callers supply so the shared Responses normalization and SSE
/// parsing can keep each provider's own policy (authentication, instructions,
/// supported fields, endpoint restrictions, error classification).
#[derive(Debug, Clone)]
pub struct ResponsesCodecProvider {
    /// Provider name recorded in output provenance (e.g. `openai`, `codex`).
    pub name: &'static str,
    /// Protocol identifier recorded in output provenance.
    pub protocol: &'static str,
    /// Normalized endpoint recorded in output provenance.
    ///
    /// Empty only when no endpoint is known (e.g. a synthetic test envelope);
    /// provider-opaque replay is gated on the full provenance tuple, so an
    /// unknown endpoint never authorizes cross-endpoint forwarding.
    pub endpoint: String,
}

impl Default for ResponsesCodecProvider {
    fn default() -> Self {
        Self {
            name: "openai",
            protocol: "responses",
            endpoint: String::new(),
        }
    }
}

impl ResponsesCodecProvider {
    /// Build provenance for a provider at its resolved Responses endpoint.
    pub fn with_endpoint(
        name: &'static str,
        protocol: &'static str,
        endpoint: impl Into<String>,
    ) -> Self {
        Self {
            name,
            protocol,
            endpoint: endpoint.into(),
        }
    }
}

/// Normalize a decoded Responses response into provider-neutral structured output
/// while recording the supplied provider provenance.
///
/// Accepts the raw response envelope so compatible providers (e.g. Codex) can
/// reuse the codec without depending on private wire types.
pub fn normalize_responses_envelope(
    envelope: Value,
    provider: ResponsesCodecProvider,
) -> Result<ChatOutput, LLMError> {
    let parsed: OpenAIResponsesResponse =
        serde_json::from_value(envelope).map_err(|error| LLMError::ResponseFormatError {
            message: format!("Failed to decode Responses API response: {error}"),
            raw_response: String::new(),
        })?;
    Ok(normalize_responses_response_with(parsed, provider))
}

fn normalize_responses_response_with(
    parsed: OpenAIResponsesResponse,
    provider: ResponsesCodecProvider,
) -> ChatOutput {
    let status = parsed.status.as_deref();
    let failed = status == Some("failed");
    let incomplete = status == Some("incomplete");

    let mut items: Vec<ChatOutputItem> = Vec::new();
    for raw in &parsed.output {
        if let Some(item) = normalize_responses_output_item(raw) {
            items.push(item);
        }
    }

    let finish_reason = if failed {
        Some(FinishReason::Error)
    } else if incomplete {
        Some(incomplete_finish_reason(&parsed.incomplete_details))
    } else {
        // Completed responses with supported local calls indicate pending tool
        // execution; otherwise they stopped.
        let has_local_calls = items
            .iter()
            .any(|item| matches!(item, ChatOutputItem::FunctionCall(_)));
        Some(if has_local_calls {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        })
    };

    let output_status = match status {
        Some("failed") => Some(ChatOutputStatus::Failed),
        Some("incomplete") => Some(ChatOutputStatus::Incomplete),
        Some("completed") => Some(ChatOutputStatus::Completed),
        Some("in_progress") => Some(ChatOutputStatus::InProgress),
        _ => None,
    };

    let output = ChatOutput {
        response_id: parsed.id.clone(),
        items,
        status: output_status,
        usage: parsed
            .usage
            .clone()
            .map(OpenAIResponsesRawUsage::into_usage),
        finish_reason,
        provenance: Some(ChatOutputProvenance {
            provider: provider.name.to_string(),
            protocol: provider.protocol.to_string(),
            model: parsed.model.clone().unwrap_or_default(),
            endpoint: provider.endpoint.to_string(),
        }),
        extensions: Extensions::new(),
    };

    output
}

/// Map an incomplete terminal cause to a QueryMT finish reason.
fn incomplete_finish_reason(details: &Option<Value>) -> FinishReason {
    match details
        .as_ref()
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        Some("max_output_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Other,
    }
}

/// Normalize one Responses output item, retaining unknown item types as opaque.
///
/// Exposed for compatible providers (e.g. Codex) that share this codec while
/// keeping their own authentication, instructions, and error policy.
pub fn normalize_responses_output_item(raw: &Value) -> Option<ChatOutputItem> {
    let item: OpenAIResponsesOutputItem = serde_json::from_value(raw.clone()).ok()?;

    match item.item_type.as_str() {
        "reasoning" => {
            let summary: Vec<ChatReasoningPart> = item
                .summary
                .unwrap_or_default()
                .into_iter()
                .filter_map(|part| part.text)
                .filter(|text| !text.is_empty())
                .map(ChatReasoningPart::text)
                .collect();
            // Encrypted continuation stays separate from the visible summary.
            let encrypted_content = raw
                .get("encrypted_content")
                .and_then(Value::as_str)
                .map(str::to_string);
            // Unknown sibling fields on the recognized item are preserved.
            let mut extensions = item.extra;
            extensions.remove("encrypted_content");
            Some(ChatOutputItem::Reasoning(ChatReasoningItem {
                id: item.id,
                summary,
                content: Vec::new(),
                encrypted_content,
                signature: None,
                status: item.status.as_deref().and_then(parse_output_status),
                extensions,
            }))
        }
        "message" => {
            let role = match item.role.as_deref() {
                None | Some("assistant") => ChatRole::Assistant,
                Some(_) => return None,
            };
            let parts = item
                .content
                .unwrap_or_default()
                .into_iter()
                .filter_map(|content| match content.content_type.as_str() {
                    "output_text" => content.text.map(|text| ChatMessagePart::Text {
                        text,
                        annotations: content
                            .annotations
                            .into_iter()
                            .map(|annotation| ChatTextAnnotation {
                                annotation_type: annotation
                                    .get("type")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                fields: annotation
                                    .as_object()
                                    .map(|object| {
                                        object
                                            .iter()
                                            .filter(|(key, _)| key.as_str() != "type")
                                            .map(|(key, value)| (key.clone(), value.clone()))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            })
                            .collect(),
                        // Unknown sibling fields on a recognized part survive.
                        extensions: content.extra,
                    }),
                    "refusal" => content.refusal.map(|refusal| ChatMessagePart::Refusal {
                        refusal,
                        extensions: content.extra,
                    }),
                    // Unknown message content kinds keep their complete payload
                    // (every field except the discriminator we already read),
                    // rather than only a literal field named `raw`.
                    _ => {
                        let mut payload = content.extra;
                        payload.insert(
                            "type".to_string(),
                            Value::String(content.content_type.clone()),
                        );
                        Some(ChatMessagePart::Opaque(ChatOpaquePart {
                            original_type: content.content_type.clone(),
                            payload: Value::Object(payload),
                        }))
                    }
                })
                .collect();
            Some(ChatOutputItem::Message(ChatMessageItem {
                id: item.id,
                role,
                phase: item.phase,
                status: item.status.as_deref().and_then(parse_output_status),
                parts,
                extensions: item.extra,
            }))
        }
        "function_call" => {
            let call_id = item.call_id.clone().or(item.id.clone())?;
            Some(ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: item.id,
                call_id,
                name: item.name.unwrap_or_default(),
                arguments: item.arguments.unwrap_or_default(),
                status: item.status.as_deref().and_then(parse_output_status),
                extensions: item.extra,
            }))
        }
        // Unknown provider items (e.g. built-in tool actions) stay opaque: they
        // are never promoted to message content or dispatched to the executor.
        _ => Some(ChatOutputItem::Opaque(ChatOpaqueItem {
            original_type: item.item_type,
            payload: raw.clone(),
        })),
    }
}

/// Normalize one `response.content_part.added` payload into an indexed part.
///
/// Unknown content kinds are preserved as opaque parts so continuation data is
/// never silently dropped, matching the normalization contract.
fn normalize_output_content_part(raw: &Value) -> Option<ChatMessagePart> {
    let content_type = raw.get("type").and_then(Value::as_str)?;

    // Unknown sibling fields on a recognized part are retained, excluding the
    // fields the typed part already models.
    let extra_extensions = |known: &[&str]| -> Extensions {
        raw.as_object()
            .map(|object| {
                object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "type" && !known.contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };

    match content_type {
        "output_text" => Some(ChatMessagePart::Text {
            text: raw
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            annotations: raw
                .get("annotations")
                .and_then(Value::as_array)
                .map(|annotations| {
                    annotations
                        .iter()
                        .map(|annotation| ChatTextAnnotation {
                            annotation_type: annotation
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            fields: annotation
                                .as_object()
                                .map(|object| {
                                    object
                                        .iter()
                                        .filter(|(key, _)| key.as_str() != "type")
                                        .map(|(key, value)| (key.clone(), value.clone()))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            extensions: extra_extensions(&["text", "annotations"]),
        }),
        "refusal" => Some(ChatMessagePart::Refusal {
            refusal: raw
                .get("refusal")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            extensions: extra_extensions(&["refusal"]),
        }),
        _ => Some(ChatMessagePart::Opaque(ChatOpaquePart {
            original_type: content_type.to_string(),
            payload: raw.clone(),
        })),
    }
}

fn parse_output_status(status: &str) -> Option<ChatOutputStatus> {
    match status {
        "in_progress" => Some(ChatOutputStatus::InProgress),
        "completed" => Some(ChatOutputStatus::Completed),
        "incomplete" => Some(ChatOutputStatus::Incomplete),
        "failed" => Some(ChatOutputStatus::Failed),
        _ => None,
    }
}

/// Extract the thinking/reasoning content from a ChatMessage, if any.
fn extract_reasoning_content(msg: &ChatMessage, include: bool) -> Option<Cow<'_, str>> {
    if !include {
        return None;
    }
    msg.thinking().map(Cow::Owned)
}

fn encode_image_data_url(mime_type: &str, data: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    format!("data:{mime_type};base64,{encoded}")
}

/// Build a valid `data:` URL from a validated media type.
///
/// MIME parameters must appear before the `;base64` marker
/// (`data:image/png; charset=binary;base64,...`), never after it, otherwise the
/// marker is corrupted and the payload is no longer decodable. Parameters are
/// emitted in their parsed form so their semantics survive.
fn encode_data_url(media_type: &querymt::chat::MediaType, data: &[u8]) -> String {
    let mut head = format!("{}", media_type.type_());
    head.push('/');
    head.push_str(media_type.subtype());
    if let Some(suffix) = media_type.suffix() {
        head.push('+');
        head.push_str(suffix);
    }
    for (name, value) in media_type.params() {
        head.push_str("; ");
        head.push_str(name);
        head.push('=');
        head.push_str(value);
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(data);
    format!("data:{head};base64,{encoded}")
}

fn text_message_content<'a>(text: impl Into<Cow<'a, str>>) -> MessageContent<'a> {
    MessageContent {
        message_type: Some(Cow::Borrowed("text")),
        text: Some(text.into()),
        image_url: None,
        tool_call_id: None,
        tool_output: None,
    }
}

fn image_url_message_content<'a>(url: impl Into<Cow<'a, str>>) -> MessageContent<'a> {
    MessageContent {
        message_type: Some(Cow::Borrowed("image_url")),
        text: None,
        image_url: Some(ImageUrlContent { url: url.into() }),
        tool_call_id: None,
        tool_output: None,
    }
}

fn image_content_block<'a>(mime_type: &str, data: &[u8]) -> MessageContent<'a> {
    image_url_message_content(encode_image_data_url(mime_type, data))
}

/// Concatenate text from canonical input parts with bounded attachment markers.
fn input_parts_text_with_fallbacks<'a>(
    parts: impl IntoIterator<Item = &'a ChatInputPart>,
) -> String {
    parts
        .into_iter()
        .filter_map(|part| match part {
            ChatInputPart::Text { text } => Some(text.clone()),
            ChatInputPart::Attachment(media) => match media.source() {
                MediaSource::Inline { data } => Some(format!(
                    "[Attachment: {}, {} bytes]",
                    media
                        .media_type()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    data.len()
                )),
                MediaSource::DataUrl { url } | MediaSource::Url { url } => {
                    Some(format!("[Attached resource: {url}]"))
                }
                MediaSource::ProviderFile { file_id, .. } => {
                    Some(format!("[Provider file: {file_id}]"))
                }
            },
            ChatInputPart::ToolResult(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render canonical tool-result parts as text, using an explicit image marker
/// when the result carries only images (matching legacy behavior).
fn canonical_tool_result_text(parts: &[ToolResultPart]) -> String {
    let text = parts
        .iter()
        .filter_map(ToolResultPart::as_text)
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty()
        && parts.iter().any(|part| {
            matches!(
                part,
                ToolResultPart::Attachment(media)
                    if media.kind == MediaKind::Image
                        && matches!(
                            media.source(),
                            MediaSource::Inline { .. }
                                | MediaSource::DataUrl { .. }
                                | MediaSource::Url { .. }
                        )
            )
        })
    {
        "[Image attached below]".to_string()
    } else {
        text
    }
}

/// Convert a ChatMessage with its canonical payload into one or more OpenAI API messages.
///
/// Most messages map 1:1, but tool-result input parts each become a separate
/// `role: "tool"` message.
fn convert_chat_message_to_openai<'a>(
    chat_msg: &'a ChatMessage,
    out: &mut Vec<OpenAIChatMessage<'a>>,
    include_reasoning: bool,
) {
    let role: Cow<'a, str> = match chat_msg.role {
        ChatRole::User => Cow::Borrowed("user"),
        ChatRole::Assistant => Cow::Borrowed("assistant"),
    };

    let parts = chat_msg.portable_input_parts();

    // Tool results must be emitted as separate `role: "tool"` messages.
    let has_tool_results = parts.iter().any(ChatInputPart::is_tool_result);
    // Generated function calls map to `tool_calls` on an assistant message.
    let has_tool_use = chat_msg.has_tool_use();

    // Emit tool result parts as separate messages. Any supplemental text on the
    // same ChatMessage is request context associated with the result batch. Keep
    // it inside the final tool response: some OpenAI-compatible APIs reject a
    // user message interleaved between an assistant tool call and its responses.
    if has_tool_results {
        let supplemental_text =
            input_parts_text_with_fallbacks(parts.iter().filter(|part| !part.is_tool_result()));
        let last_tool_result_index = parts.iter().rposition(ChatInputPart::is_tool_result);
        let mut vision_blocks = Vec::new();

        for (index, part) in parts.iter().enumerate() {
            if let ChatInputPart::ToolResult(result) = part {
                let mut text = canonical_tool_result_text(&result.parts);
                if Some(index) == last_tool_result_index && !supplemental_text.is_empty() {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&supplemental_text);
                }
                out.push(OpenAIChatMessage {
                    role: Cow::Borrowed("tool"),
                    tool_call_id: Some(Cow::Owned(result.call_id.clone())),
                    tool_calls: None,
                    content: Some(Right(Cow::Owned(text))),
                    reasoning_content: None,
                });

                let images = collect_tool_result_images(&result.parts);
                if !images.is_empty() {
                    vision_blocks.push(text_message_content(format!(
                        "[Tool result image for {}]",
                        result.call_id
                    )));
                    vision_blocks.extend(images);
                }
            }
        }

        if !vision_blocks.is_empty() {
            out.push(OpenAIChatMessage {
                role: Cow::Borrowed("user"),
                tool_call_id: None,
                tool_calls: None,
                content: Some(Left(vision_blocks)),
                reasoning_content: None,
            });
        }
        return;
    }

    // Emit generated function calls as tool_calls on an assistant message.
    if has_tool_use {
        let text = input_parts_text_with_fallbacks(parts.iter());
        let content_val = if text.is_empty() {
            None
        } else {
            Some(Right(Cow::Owned(text)))
        };

        let mut tool_calls: Vec<OpenAIFunctionCall<'a>> = Vec::new();
        if let Some(output) = chat_msg.output() {
            for item in &output.items {
                if let ChatOutputItem::FunctionCall(call) = item {
                    tool_calls.push(OpenAIFunctionCall {
                        id: Cow::Owned(call.call_id.clone()),
                        content_type: Cow::Borrowed("function"),
                        function: OpenAIFunctionPayload {
                            name: Cow::Owned(call.name.clone()),
                            arguments: Cow::Owned(call.arguments.clone()),
                        },
                    });
                }
            }
        }

        out.push(OpenAIChatMessage {
            role,
            tool_call_id: None,
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            content: content_val,
            reasoning_content: extract_reasoning_content(chat_msg, include_reasoning),
        });
        return;
    }

    // Use content-array format whenever the message contains non-text input so
    // ordering is retained and unsupported binary kinds receive explicit markers.
    let has_structured_content = parts
        .iter()
        .any(|part| matches!(part, ChatInputPart::Attachment(_)));

    if has_structured_content {
        let content_blocks: Vec<MessageContent<'static>> = parts
            .iter()
            .filter_map(|part| match part {
                ChatInputPart::Text { text } => {
                    Some(text_message_content(Cow::Owned(text.clone())))
                }
                ChatInputPart::Attachment(media) => match media.source() {
                    MediaSource::Inline { data } => {
                        if media.kind == MediaKind::Image {
                            Some(image_content_block_owned(media, data))
                        } else {
                            Some(text_message_content(format!(
                                "[Attachment: {} bytes]",
                                data.len()
                            )))
                        }
                    }
                    MediaSource::DataUrl { url } | MediaSource::Url { url } => {
                        Some(image_url_message_content(Cow::Owned(url.clone())))
                    }
                    MediaSource::ProviderFile { .. } => {
                        Some(text_message_content("[Provider attachment]"))
                    }
                },
                ChatInputPart::ToolResult(_) => None,
            })
            .collect();

        out.push(OpenAIChatMessage {
            role,
            tool_call_id: None,
            tool_calls: None,
            content: Some(Left(content_blocks)),
            reasoning_content: extract_reasoning_content(chat_msg, include_reasoning),
        });
    } else {
        // Simple text-only message
        let text = chat_msg.text();
        out.push(OpenAIChatMessage {
            role,
            tool_call_id: None,
            tool_calls: None,
            content: Some(Right(Cow::Owned(text))),
            reasoning_content: extract_reasoning_content(chat_msg, include_reasoning),
        });
    }
}

/// Collect typed image attachment parts from a canonical tool result.
fn collect_tool_result_images(parts: &[ToolResultPart]) -> Vec<MessageContent<'static>> {
    parts
        .iter()
        .filter_map(|part| match part {
            ToolResultPart::Attachment(media) if media.kind == MediaKind::Image => {
                match media.source() {
                    MediaSource::Inline { data } => Some(image_content_block_owned(media, data)),
                    MediaSource::DataUrl { url } | MediaSource::Url { url } => {
                        Some(image_url_message_content(Cow::Owned(url.clone())))
                    }
                    MediaSource::ProviderFile { .. } => None,
                }
            }
            _ => None,
        })
        .collect()
}

/// Build an image content block from a validated canonical attachment.
fn image_content_block_owned(media: &MediaPart, data: &[u8]) -> MessageContent<'static> {
    let mime_type = media
        .media_type()
        .map(ToString::to_string)
        .unwrap_or_else(|| "application/octet-stream".to_string());
    image_content_block(&mime_type, data)
}

pub fn openai_list_models_request(
    base_url: &Url,
    cfg: &Value,
) -> Result<Request<Vec<u8>>, LLMError> {
    let api_key = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let auth_type = cfg
        .get("auth_type")
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let effective_auth = determine_effective_auth(&api_key, auth_type.as_ref(), base_url)?;
    if !api_key.is_empty() {
        println!(
            "OpenAI auth debug (list models): host={}, auth_type={:?}, effective_auth={:?}, token_hint={}",
            base_url.host_str().unwrap_or("<none>"),
            auth_type,
            effective_auth,
            token_hint(&api_key)
        );
    }

    let model_list_url = base_url.join("models")?;
    let builder = Request::builder()
        .method(Method::GET)
        .uri(model_list_url.to_string())
        .header(CONTENT_TYPE, "application/json");

    let builder = maybe_add_auth_header(builder, &effective_auth, &api_key)?;
    Ok(builder.body(Vec::new())?)
}

pub fn openai_parse_list_models(response: &Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
    let error_response = response.clone();
    handle_http_error!(error_response);

    let resp_json: Value = serde_json::from_slice(response.body())?;
    let arr = resp_json
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LLMError::InvalidRequest("`data` missing or not an array".into()))?;

    let names = arr
        .iter()
        .filter_map(|m| m.get("id"))
        .filter_map(Value::as_str)
        .map(String::from)
        .collect();

    Ok(names)
}

// ============================================================================
// Streaming Support
// ============================================================================

/// Streaming response chunk from OpenAI's API
#[derive(Deserialize, Debug)]
struct OpenAIStreamChunk {
    pub choices: Vec<OpenAIStreamChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<OpenAIRawUsage>,
}

/// Individual choice in a streaming response
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamChoice {
    pub index: usize,
    pub delta: OpenAIStreamDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Delta content in a streaming response
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "reasoning",
        alias = "reasoning_content"
    )]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAIStreamToolCall>>,
}

/// Tool call in a streaming response (fields are optional for incremental updates)
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub call_type: Option<String>,
    pub function: OpenAIStreamFunction,
}

/// Function call in a streaming response
#[derive(Deserialize, Debug)]
pub struct OpenAIStreamFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Arguments are always present but may be an empty string
    #[serde(default)]
    pub arguments: String,
}

/// State for tracking incremental tool call assembly
#[derive(Default, Debug)]
pub struct OpenAIToolUseState {
    pub id: String,
    pub name: String,
    pub arguments_buffer: String,
    pub started: bool,
}

/// Normalize a vendor error `code`/`type` token for table lookup.
fn normalize_error_token(token: &str) -> String {
    token.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

/// Map a normalized OpenAI-compatible error `code`/`type` to a unified kind.
///
/// Dialect table for chat-completions / OpenAI-compatible providers. Kept in
/// this crate on purpose — core must not know vendor code strings. Codex has
/// its own responses-api table.
fn openai_error_kind(code: &str) -> Option<ProviderErrorKind> {
    match code {
        // Chat Completions + common openai-compatible dialects.
        "server_is_overloaded" | "slow_down" | "overloaded_error" | "overloaded" => {
            Some(ProviderErrorKind::ServerOverloaded)
        }
        "rate_limit_exceeded"
        | "rate_limit_error"
        | "rate_limited"
        | "rate_limit"
        | "too_many_requests" => Some(ProviderErrorKind::RateLimited),
        "context_length_exceeded" | "context_length_error" | "context_length" => {
            Some(ProviderErrorKind::ContextWindowExceeded)
        }
        // usage_limit_reached: account/plan cap (chatgpt-style); same permanent
        // bucket as insufficient_quota so openai-compatible paths (incl. xai chat)
        // do not retry plan caps as TPM rate limits.
        "insufficient_quota" | "usage_not_included" | "usage_limit_reached" => {
            Some(ProviderErrorKind::QuotaExceeded)
        }
        "invalid_request"
        | "invalid_request_error"
        | "invalid_prompt"
        | "bio_policy"
        | "cyber_policy" => Some(ProviderErrorKind::InvalidRequest),
        "authentication_error" | "invalid_api_key" | "unauthorized" => {
            Some(ProviderErrorKind::Authentication)
        }
        _ => None,
    }
}

/// Provider-specific classification layered over the shared OpenAI-compatible
/// envelope parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAIErrorClassification {
    pub kind: ProviderErrorKind,
    pub error_type: Option<String>,
}

pub type OpenAIErrorClassifier = fn(&Value) -> Option<OpenAIErrorClassification>;

/// Map an OpenAI-compatible SSE/HTTP `{ "error": ... }` envelope into unified
/// [`ProviderFailure`] kinds. Provider-specific classifiers take precedence over
/// the generic `code`/`type` dialect table.
fn map_openai_error_envelope(
    error: &Value,
    envelope: &Value,
    explicit_request_id: Option<&str>,
    unknown_transient: bool,
    provider_classifier: Option<OpenAIErrorClassifier>,
) -> ProviderFailure {
    let message = error
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .map(|message| message.trim().to_owned())
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "openai response failed".to_owned());
    let code = error.get("code").and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_i64().map(|number| number.to_string()))
    });
    let provider_classification = provider_classifier.and_then(|classify| classify(error));
    let error_type = provider_classification
        .as_ref()
        .and_then(|classification| classification.error_type.clone())
        .or_else(|| {
            error
                .get("type")
                .or_else(|| error.get("error_type"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let request_id = explicit_request_id.map(str::to_owned).or_else(|| {
        error
            .get("request_id")
            .or_else(|| error.get("requestId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let retry_after_secs =
        extract_retry_after_from_json(error).or_else(|| extract_retry_after_from_json(envelope));
    let code_norm = code.as_deref().map(normalize_error_token);
    let type_norm = error_type.as_deref().map(normalize_error_token);
    let mapped = provider_classification
        .map(|classification| classification.kind)
        .or_else(|| code_norm.as_deref().and_then(openai_error_kind))
        .or_else(|| type_norm.as_deref().and_then(openai_error_kind));

    let server_side = matches!(
        code_norm.as_deref(),
        Some("server_error" | "internal_server_error")
    ) || matches!(
        type_norm.as_deref(),
        Some("server_error" | "internal_server_error")
    );
    let kind = mapped.unwrap_or(if unknown_transient || server_side {
        ProviderErrorKind::UnknownTransient
    } else {
        ProviderErrorKind::UnknownPermanent
    });
    // Structured payload hints are authoritative. Message parsing is only a fallback
    // for rate-limit envelopes that omit machine-readable delay metadata.
    let retry_after_secs = if kind == ProviderErrorKind::RateLimited {
        retry_after_secs.or_else(|| parse_retry_after_from_message(&message))
    } else {
        retry_after_secs
    };

    ProviderFailure::new(kind, message)
        .with_code(code)
        .with_error_type(error_type)
        .with_request_id(request_id)
        .with_retry_after_secs(retry_after_secs)
}

/// Classify an OpenAI-compatible HTTP error body.
pub fn classify_openai_http_error(response: &Response<Vec<u8>>) -> LLMError {
    classify_openai_http_error_with(response, None)
}

pub fn classify_openai_http_error_with(
    response: &Response<Vec<u8>>,
    provider_classifier: Option<OpenAIErrorClassifier>,
) -> LLMError {
    let status = response.status().as_u16();
    let retry_after_secs = parse_retry_after(response.headers());
    let request_id = response
        .headers()
        .get("x-request-id")
        .or_else(|| response.headers().get("request-id"))
        .and_then(|value| value.to_str().ok());
    let envelope = serde_json::from_slice::<Value>(response.body()).ok();
    if let Some(envelope) = envelope.as_ref()
        && let Some(error) = envelope.get("error")
    {
        let mapped = map_openai_error_envelope(
            error,
            envelope,
            request_id,
            matches!(status, 429 | 500..=599),
            provider_classifier,
        );
        let retry_after_secs = retry_after_secs.or(mapped.retry_after_secs());
        return mapped.with_retry_after_secs(retry_after_secs).into();
    }

    // No vendor envelope: classify from the status alone.
    querymt::error::classify_status_only(status, response.headers(), response.body())
}

pub fn parse_openai_sse_chunk(
    chunk: &[u8],
    tool_states: &mut HashMap<usize, OpenAIToolUseState>,
) -> Result<Vec<StreamChunk>, LLMError> {
    parse_openai_sse_chunk_with(chunk, tool_states, None)
}

pub fn parse_openai_sse_chunk_with(
    chunk: &[u8],
    tool_states: &mut HashMap<usize, OpenAIToolUseState>,
    provider_classifier: Option<OpenAIErrorClassifier>,
) -> Result<Vec<StreamChunk>, LLMError> {
    // Skip empty chunks
    if chunk.is_empty() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(chunk);
    let mut results = Vec::new();
    let mut done_emitted = false;

    for line in text.lines() {
        // Stop processing if we've already emitted Done
        if done_emitted {
            break;
        }

        let line = line.trim();

        // Skip empty lines
        if line.is_empty() {
            continue;
        }

        // Extract SSE data payload
        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue, // Skip non-data lines
        };

        // Handle stream end
        if data == "[DONE]" {
            // Emit remaining tool completions
            for (index, state) in tool_states.drain() {
                if state.started {
                    results.push(StreamChunk::ToolUseComplete {
                        index,
                        tool_call: ToolCall {
                            id: state.id,
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: state.name,
                                arguments: state.arguments_buffer,
                            },
                        },
                    });
                }
            }
            results.push(StreamChunk::Done {
                finish_reason: FinishReason::Stop,
            });
            done_emitted = true;
            continue;
        }

        // Parse once so provider error envelopes and normal stream chunks share the same payload.
        let envelope: Value =
            serde_json::from_str(data).map_err(|e| LLMError::ResponseFormatError {
                message: format!("Failed to parse OpenAI stream chunk: {}", e),
                raw_response: data.to_string(),
            })?;
        if let Some(error) = envelope.get("error") {
            let explicit_request_id = envelope
                .get("request_id")
                .or_else(|| envelope.get("requestId"))
                .and_then(Value::as_str);
            return Err(map_openai_error_envelope(
                error,
                &envelope,
                explicit_request_id,
                false,
                provider_classifier,
            )
            .into());
        }
        let mut stream_chunk: OpenAIStreamChunk =
            serde_json::from_value(envelope).map_err(|e| LLMError::ResponseFormatError {
                message: format!("Failed to parse OpenAI stream chunk: {}", e),
                raw_response: data.to_string(),
            })?;

        // Emit usage metadata BEFORE Done so consumers that break on Done
        // still see Usage.  Many OpenAI-compatible APIs (Z.AI, DeepSeek, etc.)
        // include usage in the same SSE line as finish_reason.
        if let Some(usage) = stream_chunk.usage.take() {
            results.push(StreamChunk::Usage(usage.into_usage()));
        }

        // Process each choice
        for choice in &stream_chunk.choices {
            // Handle thinking/reasoning content deltas.
            if let Some(thinking) = &choice.delta.thinking
                && !thinking.is_empty()
            {
                results.push(StreamChunk::Thinking(thinking.clone()));
            }

            // Handle text content
            if let Some(content) = &choice.delta.content
                && !content.is_empty()
            {
                results.push(StreamChunk::Text(content.clone()));
            }

            // Handle tool calls
            if let Some(tool_calls) = &choice.delta.tool_calls {
                for tc in tool_calls {
                    let index = tc.index.unwrap_or(0);
                    let state = tool_states.entry(index).or_default();

                    // First chunk: has id and name
                    if let Some(id) = &tc.id {
                        state.id = id.clone();
                    }
                    if let Some(name) = &tc.function.name {
                        state.name = name.clone();

                        // Emit ToolUseStart on first occurrence
                        if !state.started {
                            state.started = true;
                            results.push(StreamChunk::ToolUseStart {
                                index,
                                id: state.id.clone(),
                                name: state.name.clone(),
                            });
                        }
                    }

                    // Accumulate arguments
                    if !tc.function.arguments.is_empty() {
                        state.arguments_buffer.push_str(&tc.function.arguments);
                        results.push(StreamChunk::ToolUseInputDelta {
                            index,
                            partial_json: tc.function.arguments.clone(),
                        });
                    }
                }
            }

            // Handle finish_reason
            if let Some(finish_reason) = &choice.finish_reason {
                // Emit tool completions before done
                for (index, state) in tool_states.drain() {
                    if state.started {
                        results.push(StreamChunk::ToolUseComplete {
                            index,
                            tool_call: ToolCall {
                                id: state.id,
                                call_type: "function".to_string(),
                                function: FunctionCall {
                                    name: state.name,
                                    arguments: state.arguments_buffer,
                                },
                            },
                        });
                    }
                }

                // Map finish_reason to FinishReason
                let finish_reason = match finish_reason.as_str() {
                    "tool_calls" => FinishReason::ToolCalls,
                    "stop" => FinishReason::Stop,
                    "length" => FinishReason::Length,
                    "content_filter" => FinishReason::ContentFilter,
                    _ => FinishReason::Unknown,
                };

                results.push(StreamChunk::Done { finish_reason });
                done_emitted = true;
            }
        }
    }

    Ok(results)
}

/// Map unified `ReasoningEffort` to the OpenAI API string.
///
/// OpenAI/Codex APIs use `"xhigh"` where the unified enum uses `Max`.
/// `None` is handled by the caller (omit field = provider/model default).
pub(crate) fn openai_effort_str(e: ReasoningEffort) -> &'static str {
    match e {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Max => "xhigh",
    }
}

// ============================================================================
// Responses streaming (semantic SSE)
// ============================================================================

/// A decoded Responses SSE event line. Unknown event kinds are ignored by kind
/// rather than being coerced into semantic events.
#[derive(Deserialize, Debug)]
struct OpenAIResponsesSseEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    item: Option<Value>,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    content_index: Option<usize>,
    #[serde(default)]
    summary_index: Option<usize>,
    /// Payload of `response.content_part.added` (an output/refusal content part)
    /// and `response.reasoning_summary_part.added` (a reasoning summary part).
    #[serde(default)]
    part: Option<Value>,
    #[serde(default)]
    response: Option<Value>,
}

/// Request-local state for a Responses stream attempt.
///
/// Tracks the identities started during this request so semantic events can
/// reference stable output indexes, and remembers whether any local function
/// call was seen for terminal finish-reason selection.
#[derive(Default)]
pub struct OpenAIResponsesStreamState {
    response_started: bool,
    response_id: Option<String>,
    model: Option<String>,
    saw_function_call: bool,
    terminal_seen: bool,
    /// Provider provenance recorded on emitted metadata events. Defaults to
    /// OpenAI; compatible providers (e.g. Codex) supply their own identity.
    provider: ResponsesCodecProvider,
}

impl OpenAIResponsesStreamState {
    /// Create request-local stream state that records the supplied provider
    /// provenance instead of the default OpenAI identity.
    pub fn for_provider(provider: ResponsesCodecProvider) -> Self {
        Self {
            provider,
            ..Self::default()
        }
    }
}

/// Policy hooks a compatible provider supplies when reusing the shared
/// Responses stream parser.
///
/// Provider-specific behavior (auth, instructions, error classification, or an
/// extra `end_turn` gate) stays with the caller; the shared parser only owns
/// item-aware normalization so structured continuation state is never flattened.
#[derive(Default)]
pub struct ResponsesStreamPolicy {
    /// Reject a completed response that reports `end_turn=false`.
    pub reject_end_turn_false: bool,
}

/// Parse Responses SSE frames using the shared item-aware codec.
///
/// This is the entry point compatible providers (Codex, xAI) use so that a
/// streamed response yields `StructuredStreamEvent`s — retaining encrypted
/// reasoning, item IDs, opaque items, and byte-exact arguments — instead of
/// being flattened into lossy legacy chunks. Legacy chunks are emitted only
/// when a provider explicitly projects them; this function never does.
pub fn parse_openai_responses_sse_chunk_with_policy(
    chunk: &[u8],
    state: &mut OpenAIResponsesStreamState,
    policy: &ResponsesStreamPolicy,
) -> Result<Vec<StreamChunk>, LLMError> {
    if policy.reject_end_turn_false && response_reports_end_turn_false(chunk) {
        state.terminal_seen = true;
        return Err(LLMError::InvalidRequest(
            "response.completed with end_turn=false is not supported".to_string(),
        ));
    }
    parse_openai_responses_sse_chunk(chunk, state)
}

/// Whether this SSE chunk contains a `response.completed` with `end_turn=false`.
fn response_reports_end_turn_false(chunk: &[u8]) -> bool {
    let text = String::from_utf8_lossy(chunk);
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .any(|event| {
            event.get("type").and_then(Value::as_str) == Some("response.completed")
                && event.get("response").and_then(|r| r.get("end_turn"))
                    == Some(&Value::Bool(false))
        })
}

/// Parse Responses API SSE frames into semantic structured stream events.
///
/// Transport framing (`[DONE]`) is ignored; completion is decided only by
/// `response.completed`. Item snapshots come from `response.output_item.done`
/// and `response.completed`, while deltas only update provisional state in the
/// accumulator.
pub fn parse_openai_responses_sse_chunk(
    chunk: &[u8],
    state: &mut OpenAIResponsesStreamState,
) -> Result<Vec<StreamChunk>, LLMError> {
    if chunk.is_empty() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(chunk);
    let mut results = Vec::new();

    for line in text.lines() {
        if state.terminal_seen {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        // Framing-only marker: it does not describe response semantics.
        if data == "[DONE]" {
            continue;
        }

        let event: OpenAIResponsesSseEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => {
                // Ignore unparseable framing lines rather than aborting the stream.
                continue;
            }
        };

        match event.kind.as_str() {
            "response.created" | "response.in_progress" => {
                if !state.response_started {
                    state.response_started = true;
                    if let Some(response) = &event.response {
                        state.response_id = response
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        state.model = response
                            .get("model")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    results.push(StreamChunk::Structured(
                        StructuredStreamEvent::ResponseMetadata {
                            response_id: state.response_id.clone(),
                            status: Some(ChatOutputStatus::InProgress),
                            usage: None,
                            finish_reason: None,
                            provenance: Some(ChatOutputProvenance {
                                provider: state.provider.name.to_string(),
                                protocol: state.provider.protocol.to_string(),
                                model: state.model.clone().unwrap_or_default(),
                                endpoint: state.provider.endpoint.to_string(),
                            }),
                        },
                    ));
                }
            }
            "response.output_item.added" => {
                let (Some(item), Some(output_index)) = (&event.item, event.output_index) else {
                    continue;
                };
                if let Some(normalized) = normalize_responses_output_item(item) {
                    if matches!(normalized, ChatOutputItem::FunctionCall(_)) {
                        state.saw_function_call = true;
                    }
                    results.push(StreamChunk::Structured(
                        StructuredStreamEvent::ItemStarted {
                            output_index,
                            item: normalized,
                        },
                    ));
                }
            }
            "response.content_part.added" => {
                let (Some(output_index), Some(content_index)) =
                    (event.output_index, event.content_index)
                else {
                    continue;
                };
                let Some(part) = event.part.as_ref().and_then(normalize_output_content_part) else {
                    continue;
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::MessagePartStarted {
                        output_index,
                        content_index,
                        part,
                    },
                ));
            }
            "response.reasoning_summary_part.added" => {
                let (Some(output_index), Some(part_index)) =
                    (event.output_index, event.summary_index)
                else {
                    continue;
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::ReasoningPartStarted {
                        output_index,
                        part: ReasoningPartKind::Summary,
                        part_index,
                    },
                ));
            }
            "response.output_item.done" => {
                let (Some(item), Some(output_index)) = (&event.item, event.output_index) else {
                    continue;
                };
                if let Some(normalized) = normalize_responses_output_item(item) {
                    if matches!(normalized, ChatOutputItem::FunctionCall(_)) {
                        state.saw_function_call = true;
                    }
                    results.push(StreamChunk::Structured(
                        StructuredStreamEvent::ItemCompleted {
                            output_index,
                            item: normalized,
                        },
                    ));
                }
            }
            "response.output_text.delta" => {
                let (Some(delta), Some(output_index)) = (&event.delta, event.output_index) else {
                    continue;
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::MessagePartDelta {
                        output_index,
                        content_index: event.content_index.unwrap_or(0),
                        delta: ChatMessagePartDelta::Text {
                            delta: delta.clone(),
                        },
                    },
                ));
            }
            "response.refusal.delta" => {
                let (Some(delta), Some(output_index)) = (&event.delta, event.output_index) else {
                    continue;
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::MessagePartDelta {
                        output_index,
                        content_index: event.content_index.unwrap_or(0),
                        delta: ChatMessagePartDelta::Refusal {
                            delta: delta.clone(),
                        },
                    },
                ));
            }
            "response.reasoning_summary_text.delta" => {
                let (Some(delta), Some(output_index)) = (&event.delta, event.output_index) else {
                    continue;
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::ReasoningPartDelta {
                        output_index,
                        part: ReasoningPartKind::Summary,
                        part_index: event.summary_index.unwrap_or(0),
                        delta: delta.clone(),
                    },
                ));
            }
            "response.reasoning_text.delta" => {
                let (Some(delta), Some(output_index)) = (&event.delta, event.output_index) else {
                    continue;
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::ReasoningPartDelta {
                        output_index,
                        part: ReasoningPartKind::Content,
                        part_index: event.summary_index.unwrap_or(0),
                        delta: delta.clone(),
                    },
                ));
            }
            "response.function_call_arguments.delta" => {
                let (Some(delta), Some(output_index)) = (&event.delta, event.output_index) else {
                    continue;
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::FunctionArgumentsDelta {
                        output_index,
                        delta: delta.clone(),
                    },
                ));
            }
            // The authoritative function call arrives via output_item.done.
            "response.function_call_arguments.done" => {}
            "response.completed" => {
                state.terminal_seen = true;
                let response = event.response.unwrap_or(Value::Null);
                let usage = response
                    .get("usage")
                    .and_then(|usage| {
                        serde_json::from_value::<OpenAIResponsesRawUsage>(usage.clone()).ok()
                    })
                    .map(OpenAIResponsesRawUsage::into_usage);
                // Reconcile the authoritative final snapshot BEFORE deriving the
                // finish reason: an `output_item.done` may be absent (or differ
                // from the final response), and a function call discovered only
                // in the final snapshot must still classify as `ToolCalls` so it
                // is dispatched exactly once. The accumulator resolves duplicate
                // and conflicting snapshots with its idempotency/conflict checks.
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    for (output_index, raw) in items.iter().enumerate() {
                        if let Some(normalized) = normalize_responses_output_item(raw) {
                            if matches!(normalized, ChatOutputItem::FunctionCall(_)) {
                                state.saw_function_call = true;
                            }
                            results.push(StreamChunk::Structured(
                                StructuredStreamEvent::ItemCompleted {
                                    output_index,
                                    item: normalized,
                                },
                            ));
                        }
                    }
                }
                let finish_reason = if state.saw_function_call {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                };
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::ResponseTerminal {
                        status: ChatOutputStatus::Completed,
                        usage,
                        finish_reason: Some(finish_reason),
                        detail: None,
                    },
                ));
            }
            "response.incomplete" => {
                state.terminal_seen = true;
                let response = event.response.unwrap_or(Value::Null);
                let reason = response
                    .get("incomplete_details")
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let usage = response
                    .get("usage")
                    .and_then(|usage| {
                        serde_json::from_value::<OpenAIResponsesRawUsage>(usage.clone()).ok()
                    })
                    .map(OpenAIResponsesRawUsage::into_usage);
                results.push(StreamChunk::Structured(
                    StructuredStreamEvent::ResponseTerminal {
                        status: ChatOutputStatus::Incomplete,
                        usage,
                        finish_reason: Some(incomplete_finish_reason(
                            &response.get("incomplete_details").cloned(),
                        )),
                        detail: Some(reason.to_string()),
                    },
                ));
            }
            "response.failed" => {
                state.terminal_seen = true;
                let response = event.response.unwrap_or(Value::Null);
                let error = response.get("error").unwrap_or(&Value::Null);
                let provider_error = map_openai_error_envelope(error, &response, None, true, None);
                return Err(provider_error.into());
            }
            _ => {}
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use http::Response;
    use querymt::{
        chat::{ChatOutput, StreamChunk},
        error::{LLMError, ProviderErrorKind},
    };
    use std::collections::HashMap;

    use super::{
        MultipartForm, OpenAIChatResponse, OpenAIToolUseState, classify_openai_http_error,
        convert_chat_message_to_openai, convert_structured_output_to_responses, openai_parse_chat,
        openai_parse_list_models, parse_openai_sse_chunk,
    };
    use crate::OpenAI;

    #[test]
    fn responses_replay_reasoning_without_summary_serializes_empty_summary_array() {
        use querymt::chat::{ChatOutputItem, ChatOutputProvenance, ChatReasoningItem, Extensions};

        let output = ChatOutput {
            provenance: Some(ChatOutputProvenance {
                provider: "openai".to_string(),
                protocol: "responses".to_string(),
                model: "gpt-5".to_string(),
                endpoint: "https://api.openai.com/v1/responses".to_string(),
            }),
            items: vec![ChatOutputItem::Reasoning(ChatReasoningItem {
                id: Some("rs_1".to_string()),
                summary: Vec::new(),
                content: Vec::new(),
                encrypted_content: Some("enc_payload".to_string()),
                signature: None,
                status: None,
                extensions: Extensions::new(),
            })],
            ..ChatOutput::default()
        };

        let mut out = Vec::new();
        convert_structured_output_to_responses(&output, &mut out, true)
            .expect("empty-summary reasoning must replay");
        assert_eq!(out.len(), 1);
        let value = serde_json::to_value(&out[0]).unwrap();

        assert_eq!(value["type"], "reasoning");
        // The Responses API requires `summary` on every reasoning input item,
        // even when the model emitted no summary text.
        assert_eq!(value["summary"], serde_json::json!([]));
        assert_eq!(value["id"], "rs_1");
        assert_eq!(value["encrypted_content"], "enc_payload");
    }

    #[test]
    fn raw_images_serialize_as_ordered_data_urls() {
        use querymt::chat::ChatMessage;

        let message = ChatMessage::user()
            .text("before")
            .image("image/png".parse().unwrap(), vec![1, 2, 3])
            .text("after")
            .build();
        let mut converted = Vec::new();
        convert_chat_message_to_openai(&message, &mut converted, true);
        let value = serde_json::to_value(&converted[0]).unwrap();
        let content = value["content"].as_array().unwrap();

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["text"], "before");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AQID");
        assert_eq!(content[2]["text"], "after");
    }

    #[test]
    fn image_only_message_is_not_dropped() {
        use querymt::chat::ChatMessage;

        let message = ChatMessage::user()
            .image("image/jpeg".parse().unwrap(), vec![0xff, 0xd8])
            .build();
        let mut converted = Vec::new();
        convert_chat_message_to_openai(&message, &mut converted, true);
        let value = serde_json::to_value(&converted[0]).unwrap();

        assert_eq!(
            value["content"][0]["image_url"]["url"],
            "data:image/jpeg;base64,/9g="
        );
    }

    #[test]
    fn pdf_only_message_uses_explicit_size_marker() {
        use querymt::chat::ChatMessage;

        let message = ChatMessage::user()
            .pdf(vec![0x25, 0x50, 0x44, 0x46])
            .build();
        let mut converted = Vec::new();
        convert_chat_message_to_openai(&message, &mut converted, true);
        let value = serde_json::to_value(&converted[0]).unwrap();

        assert_eq!(value["content"].as_array().unwrap().len(), 1);
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "[Attachment: 4 bytes]");
    }

    #[test]
    fn mixed_image_pdf_and_text_preserve_order_without_filtering() {
        use querymt::chat::ChatMessage;

        let message = ChatMessage::user()
            .text("before")
            .image("image/png".parse().unwrap(), vec![1, 2, 3])
            .pdf(vec![0x25, 0x50])
            .text("after")
            .build();
        let mut converted = Vec::new();
        convert_chat_message_to_openai(&message, &mut converted, true);
        let value = serde_json::to_value(&converted[0]).unwrap();
        let content = value["content"].as_array().unwrap();

        assert_eq!(content.len(), 4);
        assert_eq!(content[0]["text"], "before");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AQID");
        assert_eq!(content[2]["text"], "[Attachment: 2 bytes]");
        assert_eq!(content[3]["text"], "after");
    }

    #[test]
    fn tool_result_pdf_uses_explicit_marker() {
        use querymt::chat::{ChatInputPart, ChatMessage, ToolResult, ToolResultPart};

        let mut result = ToolResult::new("call-1");
        result.parts.push(ToolResultPart::Text {
            text: "[Attachment: 4 bytes]".to_string(),
        });
        let message = ChatMessage::user()
            .part(ChatInputPart::tool_result(result))
            .build();
        let mut converted = Vec::new();
        convert_chat_message_to_openai(&message, &mut converted, true);
        let value = serde_json::to_value(&converted[0]).unwrap();

        assert_eq!(converted.len(), 1);
        assert_eq!(value["role"], "tool");
        assert_eq!(value["content"], "[Attachment: 4 bytes]");
    }

    #[test]
    fn tool_result_image_is_forwarded_as_follow_up_user_message() {
        use querymt::chat::{
            ChatInputPart, ChatMessage, MediaKind, MediaPart, MediaSource, ToolResult,
            ToolResultPart,
        };

        let media = MediaPart::new(
            MediaKind::Image,
            Some("image/png".parse().unwrap()),
            MediaSource::Inline {
                data: vec![1, 2, 3],
            },
        )
        .unwrap();
        let mut result = ToolResult::new("call-1");
        result
            .parts
            .push(ToolResultPart::Attachment(Box::new(media)));
        let message = ChatMessage::user()
            .part(ChatInputPart::tool_result(result))
            .build();
        let mut converted = Vec::new();
        convert_chat_message_to_openai(&message, &mut converted, true);
        let value = serde_json::to_value(&converted).unwrap();
        let messages = value.as_array().unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call-1");
        assert_eq!(messages[0]["content"], "[Image attached below]");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"][0]["text"],
            "[Tool result image for call-1]"
        );
        assert_eq!(
            messages[1]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
    }

    #[test]
    fn mixed_tool_result_keeps_text_in_tool_role_and_images_after_batch() {
        use querymt::chat::{
            ChatInputPart, ChatMessage, MediaKind, MediaPart, MediaSource, ToolResult,
            ToolResultPart,
        };

        let media = MediaPart::new(
            MediaKind::Image,
            Some("image/png".parse().unwrap()),
            MediaSource::Inline {
                data: vec![1, 2, 3],
            },
        )
        .unwrap();
        let mut first = ToolResult::new("call-1");
        first.parts.push(ToolResultPart::Text {
            text: "file exists".to_string(),
        });
        first
            .parts
            .push(ToolResultPart::Attachment(Box::new(media)));

        let message = ChatMessage::user()
            .part(ChatInputPart::tool_result(first))
            .part(ChatInputPart::tool_result(ToolResult::text("call-2", "ok")))
            .text("<run-objective>Inspect screenshot</run-objective>")
            .build();
        let mut converted = Vec::new();
        convert_chat_message_to_openai(&message, &mut converted, true);
        let value = serde_json::to_value(&converted).unwrap();
        let messages = value.as_array().unwrap();

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call-1");
        assert_eq!(messages[0]["content"], "file exists");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(
            messages[1]["content"],
            "ok\n\n<run-objective>Inspect screenshot</run-objective>"
        );
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(
            messages[2]["content"][0]["text"],
            "[Tool result image for call-1]"
        );
        assert_eq!(
            messages[2]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AQID"
        );
    }

    #[test]
    fn multipart_form_encodes_text_and_file_parts() {
        let boundary = "b";
        let mut form = MultipartForm::new(boundary);
        form.text("model", "whisper-1").unwrap();
        form.text("response_format", "json").unwrap();
        form.file("file", "audio.wav", "audio/wav", b"abc").unwrap();
        let body = form.finish();

        let s = String::from_utf8_lossy(&body);

        assert!(s.contains("--b\r\n"));
        assert!(s.contains("Content-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n"));
        assert!(
            s.contains("Content-Disposition: form-data; name=\"response_format\"\r\n\r\njson\r\n")
        );
        assert!(
            s.contains("Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n")
        );
        assert!(s.contains("Content-Type: audio/wav\r\n\r\nabc\r\n"));
        assert!(s.ends_with("--b--\r\n"));
    }

    #[test]
    fn parse_list_models_returns_model_ids_for_success_payload() {
        let response = Response::builder()
            .status(200)
            .body(br#"{"data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"}]}"#.to_vec())
            .expect("response should build");

        let models = openai_parse_list_models(&response).expect("model parsing should succeed");
        assert_eq!(models, vec!["gpt-4o", "gpt-4o-mini"]);
    }

    #[test]
    fn parse_list_models_surfaces_openai_400_message() {
        let response = Response::builder()
            .status(400)
            .body(br#"{"error":{"message":"Invalid request. Please try again later."}}"#.to_vec())
            .expect("response should build");

        let err = openai_parse_list_models(&response).expect_err("400 response should error");
        match err {
            LLMError::InvalidRequest(message) => {
                assert_eq!(message, "Invalid request. Please try again later.");
            }
            other => panic!("expected InvalidRequest, got {other}"),
        }
    }

    #[test]
    fn parse_list_models_maps_401_to_auth_error() {
        let response = Response::builder()
            .status(401)
            .body(br#"{"error":{"message":"Invalid auth token"}}"#.to_vec())
            .expect("response should build");

        let err = openai_parse_list_models(&response).expect_err("401 response should error");
        match err {
            LLMError::AuthError(message) => {
                assert_eq!(message, "Invalid auth token");
            }
            other => panic!("expected AuthError, got {other}"),
        }
    }

    #[test]
    fn tool_result_context_stays_in_the_tool_response_batch() {
        use querymt::chat::{ChatInputPart, ChatMessage};

        let messages = [
            ChatMessage::from(querymt::chat::ChatOutput {
                items: vec![
                    querymt::chat::ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                        id: None,
                        role: querymt::chat::ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![querymt::chat::ChatMessagePart::Text {
                            text: "I'll inspect the workspace.".to_string(),
                            annotations: Vec::new(),
                            extensions: Default::default(),
                        }],
                        extensions: Default::default(),
                    }),
                    querymt::chat::ChatOutputItem::FunctionCall(
                        querymt::chat::ChatFunctionCallItem {
                            item_id: None,
                            call_id: "call_1".to_string(),
                            name: "ls".to_string(),
                            arguments: "{\"path\":\".\"}".to_string(),
                            status: None,
                            extensions: Default::default(),
                        },
                    ),
                ],
                ..querymt::chat::ChatOutput::default()
            }),
            ChatMessage::user()
                .part(ChatInputPart::tool_result(
                    querymt::chat::ToolResult::new("call_1")
                        .with_name("ls")
                        .with_text("specs.md"),
                ))
                .text("<run-objective>Collect benchmark data</run-objective>")
                .build(),
        ];

        let mut converted = Vec::new();
        for message in &messages {
            super::convert_chat_message_to_openai(message, &mut converted, true);
        }
        let converted = serde_json::to_value(converted).unwrap();
        let messages = converted.as_array().unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
        assert_eq!(
            messages[1]["content"],
            "specs.md\n\n<run-objective>Collect benchmark data</run-objective>"
        );
    }

    #[test]
    fn chat_request_includes_reasoning_content_by_default() {
        use super::{OpenAIProviderConfig, openai_chat_request};
        use querymt::chat::{ChatMessage, StructuredOutputFormat, Tool, ToolChoice};
        use serde_json::Value;
        use url::Url;

        struct Cfg {
            base_url: Url,
        }

        impl OpenAIProviderConfig for Cfg {
            fn api_key(&self) -> &str {
                "test-key"
            }
            fn base_url(&self) -> &Url {
                &self.base_url
            }
            fn model(&self) -> &str {
                "gpt-test"
            }
            fn max_tokens(&self) -> Option<&u32> {
                None
            }
            fn temperature(&self) -> Option<&f32> {
                None
            }
            fn system(&self) -> &[String] {
                &[]
            }
            fn timeout_seconds(&self) -> Option<&u64> {
                None
            }
            fn stream(&self) -> Option<&bool> {
                None
            }
            fn top_p(&self) -> Option<&f32> {
                None
            }
            fn top_k(&self) -> Option<&u32> {
                None
            }
            fn tools(&self) -> Option<&[Tool]> {
                None
            }
            fn tool_choice(&self) -> Option<&ToolChoice> {
                None
            }
            fn embedding_encoding_format(&self) -> Option<&str> {
                None
            }
            fn embedding_dimensions(&self) -> Option<&u32> {
                None
            }
            fn json_schema(&self) -> Option<&StructuredOutputFormat> {
                None
            }
        }

        let cfg = Cfg {
            base_url: Url::parse("https://api.openai.com/v1/").unwrap(),
        };
        let messages = vec![
            ChatMessage::user().text("run tool").build(),
            ChatMessage::from(querymt::chat::ChatOutput {
                items: vec![
                    querymt::chat::ChatOutputItem::Reasoning(querymt::chat::ChatReasoningItem {
                        id: None,
                        summary: vec![querymt::chat::ChatReasoningPart::text("need to run tool")],
                        content: Vec::new(),
                        encrypted_content: None,
                        signature: None,
                        status: None,
                        extensions: Default::default(),
                    }),
                    querymt::chat::ChatOutputItem::FunctionCall(
                        querymt::chat::ChatFunctionCallItem {
                            item_id: None,
                            call_id: "call_1".to_string(),
                            name: "run".to_string(),
                            arguments: "{}".to_string(),
                            status: None,
                            extensions: Default::default(),
                        },
                    ),
                ],
                ..querymt::chat::ChatOutput::default()
            }),
        ];
        let request = openai_chat_request(&cfg, &messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m.get("tool_calls").is_some())
            .unwrap();
        assert_eq!(
            assistant.get("reasoning_content").and_then(Value::as_str),
            Some("need to run tool")
        );
    }

    #[test]
    fn chat_request_omits_reasoning_content_when_disabled() {
        use super::{OpenAIProviderConfig, openai_chat_request};
        use querymt::chat::{ChatMessage, StructuredOutputFormat, Tool, ToolChoice};
        use serde_json::Value;
        use url::Url;

        struct Cfg {
            base_url: Url,
        }

        impl OpenAIProviderConfig for Cfg {
            fn api_key(&self) -> &str {
                "test-key"
            }
            fn base_url(&self) -> &Url {
                &self.base_url
            }
            fn model(&self) -> &str {
                "gpt-test"
            }
            fn max_tokens(&self) -> Option<&u32> {
                None
            }
            fn temperature(&self) -> Option<&f32> {
                None
            }
            fn system(&self) -> &[String] {
                &[]
            }
            fn timeout_seconds(&self) -> Option<&u64> {
                None
            }
            fn stream(&self) -> Option<&bool> {
                None
            }
            fn top_p(&self) -> Option<&f32> {
                None
            }
            fn top_k(&self) -> Option<&u32> {
                None
            }
            fn tools(&self) -> Option<&[Tool]> {
                None
            }
            fn tool_choice(&self) -> Option<&ToolChoice> {
                None
            }
            fn embedding_encoding_format(&self) -> Option<&str> {
                None
            }
            fn embedding_dimensions(&self) -> Option<&u32> {
                None
            }
            fn include_reasoning_content(&self) -> bool {
                false
            }
            fn json_schema(&self) -> Option<&StructuredOutputFormat> {
                None
            }
        }

        let cfg = Cfg {
            base_url: Url::parse("https://api.openai.com/v1/").unwrap(),
        };
        let messages = vec![
            ChatMessage::user().text("run tool").build(),
            ChatMessage::from(querymt::chat::ChatOutput {
                items: vec![
                    querymt::chat::ChatOutputItem::Reasoning(querymt::chat::ChatReasoningItem {
                        id: None,
                        summary: vec![querymt::chat::ChatReasoningPart::text("need to run tool")],
                        content: Vec::new(),
                        encrypted_content: None,
                        signature: None,
                        status: None,
                        extensions: Default::default(),
                    }),
                    querymt::chat::ChatOutputItem::FunctionCall(
                        querymt::chat::ChatFunctionCallItem {
                            item_id: None,
                            call_id: "call_1".to_string(),
                            name: "run".to_string(),
                            arguments: "{}".to_string(),
                            status: None,
                            extensions: Default::default(),
                        },
                    ),
                ],
                ..querymt::chat::ChatOutput::default()
            }),
        ];

        let request = openai_chat_request(&cfg, &messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let assistant = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m.get("tool_calls").is_some())
            .unwrap();
        assert!(assistant.get("reasoning_content").is_none());
    }

    #[test]
    fn parse_chat_response_exposes_thinking_alias_fields() {
        let body = br#"{
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "final",
                    "reasoning": "step one"
                }
            }]
        }"#;
        let response: ChatOutput =
            ChatOutput::from(serde_json::from_slice::<OpenAIChatResponse>(body).unwrap());
        assert_eq!(response.text().as_deref(), Some("final"));
        assert_eq!(response.thinking().as_deref(), Some("step one"));

        let body_with_reasoning_content = br#"{
            "choices": [{
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "final",
                    "reasoning_content": "step two"
                }
            }]
        }"#;
        let response: ChatOutput = ChatOutput::from(
            serde_json::from_slice::<OpenAIChatResponse>(body_with_reasoning_content).unwrap(),
        );
        assert_eq!(response.thinking().as_deref(), Some("step two"));
    }

    #[test]
    fn parse_sse_chunk_emits_thinking_and_text_deltas() {
        let mut tool_states: HashMap<usize, OpenAIToolUseState> = HashMap::new();
        let chunk = br#"data: {"choices":[{"index":0,"delta":{"reasoning":"thought ","content":"answer "}}]}

data: {"choices":[{"index":0,"delta":{"reasoning_content":"continued"}}]}

"#;

        let events = parse_openai_sse_chunk(chunk, &mut tool_states).unwrap();
        assert_eq!(events.len(), 3);
        match &events[0] {
            StreamChunk::Thinking(text) => assert_eq!(text, "thought "),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
        match &events[1] {
            StreamChunk::Text(text) => assert_eq!(text, "answer "),
            other => panic!("expected text chunk, got {other:?}"),
        }
        match &events[2] {
            StreamChunk::Thinking(text) => assert_eq!(text, "continued"),
            other => panic!("expected thinking chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_non_success_uses_live_classifier_guard() {
        let response = Response::builder()
            .status(401)
            .body(br#"{"error":{"message":"bad key","code":"invalid_api_key"}}"#.to_vec())
            .unwrap();

        let config: OpenAI = serde_json::from_value(serde_json::json!({
            "model": "gpt-test"
        }))
        .unwrap();
        let error = openai_parse_chat(&config, response).unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::Authentication
                    && failure.message() == "bad key"
        ));
    }

    #[test]
    fn classify_http_499_uses_central_cancellation_mapping() {
        let response = Response::builder().status(499).body(Vec::new()).unwrap();
        assert!(matches!(
            classify_openai_http_error(&response),
            LLMError::Cancelled
        ));
    }

    #[test]
    fn classify_http_error_uses_provider_mapping_and_headers() {
        let response = Response::builder()
            .status(429)
            .header("retry-after", "4")
            .header("x-request-id", "req-http")
            .body(
                br#"{"error":{"message":"slow down","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
                    .to_vec(),
            )
            .unwrap();

        let error = classify_openai_http_error(&response);
        match error {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.message(), "slow down");
                assert_eq!(failure.kind(), ProviderErrorKind::RateLimited);
                assert_eq!(failure.request_id(), Some("req-http"));
                assert_eq!(failure.retry_after_secs(), Some(4));
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
    }

    #[test]
    fn classify_http_header_retry_hint_precedes_structured_body_hint() {
        let response = Response::builder()
            .status(429)
            .header("retry-after", "30")
            .body(
                br#"{"error":{"message":"slow down","code":"rate_limit_exceeded","retry_after":"4s"}}"#
                    .to_vec(),
            )
            .unwrap();

        let error = classify_openai_http_error(&response);
        assert_eq!(error.retry_after_secs(), Some(30));
    }

    #[test]
    fn classify_http_unknown_error_uses_status_retryability() {
        for (status, expected_retryable) in [(400, false), (429, true), (503, true)] {
            let response = Response::builder()
                .status(status)
                .body(
                    br#"{"error":{"message":"vendor failure","code":"vendor_specific"}}"#.to_vec(),
                )
                .unwrap();

            let error = classify_openai_http_error(&response);
            assert_eq!(error.is_retryable(), expected_retryable, "status={status}");
        }
    }

    #[test]
    fn parse_sse_unknown_error_without_server_evidence_is_permanent() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"vendor failure","code":"vendor_specific"}}

"#;

        let error = parse_openai_sse_chunk(chunk, &mut tool_states).unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::UnknownPermanent
        ));
        assert!(!error.is_retryable());
    }

    #[test]
    fn parse_sse_numeric_vendor_code_is_not_assumed_to_be_http_status() {
        for code in [serde_json::json!(504), serde_json::json!("504")] {
            let mut tool_states = HashMap::new();
            let chunk = format!(
                "data: {{\"error\":{{\"message\":\"vendor failure\",\"code\":{code}}}}}\n\n"
            );

            let error = parse_openai_sse_chunk(chunk.as_bytes(), &mut tool_states)
                .expect_err("error envelope should return an error");
            assert!(matches!(
                error,
                LLMError::ProviderResponseError(ref failure)
                    if failure.code() == Some("504")
                        && failure.kind() == ProviderErrorKind::UnknownPermanent
            ));
            assert!(!error.is_retryable());
        }
    }

    #[test]
    fn parse_sse_numeric_client_error_remains_permanent() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"vendor failure","code":499}}

"#;

        let error = parse_openai_sse_chunk(chunk, &mut tool_states).unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::UnknownPermanent
        ));
        assert!(!error.is_retryable());
    }

    #[test]
    fn parse_sse_chunk_returns_classified_error() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"busy","code":"server_error"}}

"#;
        let error = parse_openai_sse_chunk(chunk, &mut tool_states).unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::UnknownTransient
        ));
    }

    #[test]
    fn parse_sse_chunk_maps_server_error_as_retryable_catch_all() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"request_id":"req_123","retry_after":"3s","error":{"message":"backend unavailable","code":"server_error","type":"server_error"}}

"#;

        let error = parse_openai_sse_chunk(chunk, &mut tool_states)
            .expect_err("error envelope should return an error");
        match &error {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.message(), "backend unavailable");
                assert_eq!(failure.code(), Some("server_error"));
                assert_eq!(failure.error_type(), Some("server_error"));
                assert_eq!(failure.request_id(), Some("req_123"));
                assert_eq!(failure.retry_after_secs(), Some(3));
                assert!(failure.is_retryable());
                assert_eq!(failure.kind(), ProviderErrorKind::UnknownTransient);
            }
            other => panic!("expected ProviderResponseError, got {other}"),
        }
        assert!(error.is_retryable());
    }

    #[test]
    fn parse_sse_chunk_maps_invalid_request_as_permanent() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"unsupported field","code":"invalid_request","type":"invalid_request_error"}}

"#;

        let error = parse_openai_sse_chunk(chunk, &mut tool_states)
            .expect_err("error envelope should return an error");
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.message() == "unsupported field"
                    && failure.kind() == ProviderErrorKind::InvalidRequest
        ));
        assert!(!error.is_retryable());
    }

    #[test]
    fn parse_sse_chunk_maps_rate_limit_error() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"Rate limit reached for requests. Please try again in 2s.","type":"rate_limit_error","code":"rate_limit_exceeded"}}

"#;

        let error = parse_openai_sse_chunk(chunk, &mut tool_states)
            .expect_err("rate limit envelope should return an error");
        match &error {
            LLMError::ProviderResponseError(failure) => {
                assert!(failure.message().contains("Rate limit"));
                assert_eq!(failure.retry_after_secs(), Some(2));
                assert_eq!(failure.kind(), ProviderErrorKind::RateLimited);
            }
            other => panic!("expected rate limit, got {other}"),
        }
        assert!(error.is_retryable());
    }

    #[test]
    fn parse_sse_chunk_maps_insufficient_quota() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}

"#;

        let error = parse_openai_sse_chunk(chunk, &mut tool_states)
            .expect_err("quota envelope should return an error");
        match &error {
            LLMError::ProviderResponseError(failure) => {
                assert!(failure.message().contains("quota"));
                assert_eq!(failure.kind(), ProviderErrorKind::QuotaExceeded);
                assert_eq!(failure.code(), Some("insufficient_quota"));
            }
            other => panic!("expected QuotaExceeded, got {other}"),
        }
        assert!(!error.is_retryable());
    }

    #[test]
    fn classify_http_429_usage_limit_reached_is_permanent_quota() {
        // Same permanent bucket as codex: plan/account caps must not retry.
        let response = Response::builder()
            .status(429)
            .header(http::header::RETRY_AFTER, "60")
            .body(
                br#"{"error":{"message":"You have hit your usage limit.","type":"usage_limit_reached"}}"#
                    .to_vec(),
            )
            .unwrap();
        let error = classify_openai_http_error(&response);
        assert!(!error.is_retryable());
        assert!(!error.is_rate_limited());
        assert_eq!(error.retry_after_secs(), Some(60));
        match &error {
            LLMError::ProviderResponseError(failure) => {
                assert_eq!(failure.kind(), ProviderErrorKind::QuotaExceeded);
                assert_eq!(failure.error_type(), Some("usage_limit_reached"));
            }
            other => panic!("expected QuotaExceeded, got {other}"),
        }
    }

    #[test]
    fn parse_sse_chunk_maps_overloaded_type_to_server_overloaded() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"busy","type":"overloaded_error"}}

"#;
        let error = parse_openai_sse_chunk(chunk, &mut tool_states).unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::ServerOverloaded
        ));
        assert!(error.is_retryable());
    }

    #[test]
    fn parse_sse_chunk_falls_back_from_unknown_code_to_known_type() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"slow down","code":"vendor_specific","type":"rate_limit_error"}}

"#;
        let error = parse_openai_sse_chunk(chunk, &mut tool_states).unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::RateLimited
        ));
        assert!(error.is_retryable());
    }

    #[test]
    fn parse_sse_chunk_maps_type_only_permanent_error() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"error":{"message":"too long","type":"context_length_error"}}

"#;
        let error = parse_openai_sse_chunk(chunk, &mut tool_states).unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::ContextWindowExceeded
        ));
        assert!(!error.is_retryable());
    }

    #[test]
    fn parse_sse_chunk_preserves_response_format_error_for_invalid_normal_shape() {
        let mut tool_states = HashMap::new();
        let chunk = br#"data: {"choices":"not-an-array"}

"#;

        let error = parse_openai_sse_chunk(chunk, &mut tool_states)
            .expect_err("invalid normal chunk should return an error");
        assert!(matches!(error, LLMError::ResponseFormatError { .. }));
    }

    #[test]
    fn openai_effort_str_maps_correctly() {
        use super::{ReasoningEffort, openai_effort_str};
        assert_eq!(openai_effort_str(ReasoningEffort::Low), "low");
        assert_eq!(openai_effort_str(ReasoningEffort::Medium), "medium");
        assert_eq!(openai_effort_str(ReasoningEffort::High), "high");
        // Max must map to "xhigh" — OpenAI API does not accept "max"
        assert_eq!(openai_effort_str(ReasoningEffort::Max), "xhigh");
    }

    #[test]
    fn parse_deepseek_usage_with_cache_hit_tokens() {
        use super::OpenAIRawUsage;

        // DeepSeek returns cache info as flat top-level fields
        let json = r#"{
            "prompt_tokens": 10000,
            "completion_tokens": 5000,
            "prompt_cache_hit_tokens": 9000,
            "prompt_cache_miss_tokens": 1000,
            "total_tokens": 15000,
            "completion_tokens_details": { "reasoning_tokens": 4000 }
        }"#;
        let raw: OpenAIRawUsage = serde_json::from_str(json).unwrap();
        let usage = raw.into_usage();
        assert_eq!(usage.cache_read, 9000);
        assert_eq!(usage.input_tokens, 1000); // 10000 - 9000
        assert_eq!(usage.output_tokens, 1000); // 5000 - 4000 (reasoning)
        assert_eq!(usage.reasoning_tokens, 4000);
    }

    #[test]
    fn parse_openai_usage_with_nested_cached_tokens() {
        use super::OpenAIRawUsage;

        // Standard OpenAI format: cached_tokens nested in prompt_tokens_details
        let json = r#"{
            "prompt_tokens": 10000,
            "completion_tokens": 5000,
            "prompt_tokens_details": { "cached_tokens": 7500 },
            "completion_tokens_details": { "reasoning_tokens": 2000 }
        }"#;
        let raw: OpenAIRawUsage = serde_json::from_str(json).unwrap();
        let usage = raw.into_usage();
        assert_eq!(usage.cache_read, 7500);
        assert_eq!(usage.input_tokens, 2500); // 10000 - 7500
        assert_eq!(usage.output_tokens, 3000); // 5000 - 2000
        assert_eq!(usage.reasoning_tokens, 2000);
    }

    #[test]
    fn parse_usage_without_cache_or_reasoning() {
        use super::OpenAIRawUsage;

        // Minimal usage (no cache, no reasoning)
        let json = r#"{
            "prompt_tokens": 500,
            "completion_tokens": 100,
            "total_tokens": 600
        }"#;
        let raw: OpenAIRawUsage = serde_json::from_str(json).unwrap();
        let usage = raw.into_usage();
        assert_eq!(usage.cache_read, 0);
        assert_eq!(usage.input_tokens, 500);
        assert_eq!(usage.output_tokens, 100);
        assert_eq!(usage.reasoning_tokens, 0);
    }
}
