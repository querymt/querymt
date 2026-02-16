use either::*;
use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use querymt::{
    FunctionCall, ToolCall, Usage,
    chat::{
        ChatMessage, ChatResponse, ChatRole, FinishReason, MessageType, StreamChunk,
        StructuredOutputFormat, Tool, ToolChoice,
    },
    error::LLMError,
    handle_http_error,
    stt::{SttRequest, SttResponse},
    tts::{TtsRequest, TtsResponse},
};
use schemars::{
    r#gen::SchemaGenerator,
    schema::{InstanceType, Schema, SchemaObject, SingleOrVec},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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
    Schema::Object(SchemaObject {
        metadata: None,
        // say "this is a string"
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
        // with the "uri" format
        format: Some("uri".to_string()),
        ..Default::default()
    })
}

/// Individual message in an OpenAI chat conversation.
#[derive(Serialize, Debug)]
struct OpenAIChatMessage<'a> {
    #[allow(dead_code)]
    role: &'a str,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "either::serde_untagged_optional"
    )]
    content: Option<Either<Vec<MessageContent<'a>>, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIFunctionCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Debug)]
struct OpenAIFunctionPayload<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize, Debug)]
struct OpenAIFunctionCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    content_type: &'a str,
    function: OpenAIFunctionPayload<'a>,
}

#[derive(Serialize, Debug)]
struct MessageContent<'a> {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    message_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<ImageUrlContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "content")]
    tool_output: Option<&'a str>,
}

/// Individual image message in an OpenAI chat conversation.
#[derive(Serialize, Debug)]
struct ImageUrlContent<'a> {
    url: &'a str,
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
        let cache_read = self
            .prompt_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
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

impl ChatResponse for OpenAIChatResponse {
    fn text(&self) -> Option<String> {
        self.choices.first().and_then(|c| c.message.content.clone())
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        self.choices
            .first()
            .and_then(|c| c.message.tool_calls.clone())
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone().map(|u| u.into_usage())
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        self.choices
            .first()
            .map(|c| match c.finish_reason.as_str() {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "content_filter" => FinishReason::ContentFilter,
                "tool_calls" | "function_call" => FinishReason::ToolCalls,
                _ => FinishReason::Unknown,
            })
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
    fn reasoning_effort(&self) -> Option<&String> {
        None
    }
    fn json_schema(&self) -> Option<&StructuredOutputFormat>;
    fn extra_body(&self) -> Option<Map<String, Value>> {
        None
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
        voice: req.voice.as_deref(),
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

pub fn openai_chat_request<C: OpenAIProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Request<Vec<u8>>, LLMError> {
    let token = cfg.api_key();
    let auth = determine_effective_auth(token, cfg.auth_type(), cfg.base_url())?;

    // Clone the messages to have an owned mutable vector.
    let messages = messages.to_vec();

    let mut openai_msgs: Vec<OpenAIChatMessage> = vec![];

    for msg in messages {
        if let MessageType::ToolResult(ref results) = msg.message_type {
            for result in results {
                openai_msgs.push(
                    // Clone strings to own them
                    OpenAIChatMessage {
                        role: "tool",
                        tool_call_id: Some(result.id.clone()),
                        tool_calls: None,
                        content: Some(Right(result.function.arguments.clone())),
                    },
                );
            }
        } else {
            openai_msgs.push(chat_message_to_api_message(msg))
        }
    }

    let system_parts = cfg.system();
    if !system_parts.is_empty() {
        // Insert system messages in reverse order at position 0
        // so they end up in the correct order.
        for part in system_parts.iter().rev() {
            openai_msgs.insert(
                0,
                OpenAIChatMessage {
                    role: "system",
                    content: Some(Left(vec![MessageContent {
                        message_type: Some("text"),
                        text: Some(part),
                        image_url: None,
                        tool_call_id: None,
                        tool_output: None,
                    }])),
                    tool_calls: None,
                    tool_call_id: None,
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
        reasoning_effort: cfg.reasoning_effort().cloned(),
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

pub fn openai_parse_chat<C: OpenAIProviderConfig>(
    _cfg: &C,
    response: Response<Vec<u8>>,
) -> Result<Box<dyn ChatResponse>, LLMError> {
    // If we got a non-200 response, let's get the error details
    handle_http_error!(response);

    // Parse the successful response
    let json_resp: Result<OpenAIChatResponse, serde_json::Error> =
        serde_json::from_slice(response.body());

    let resp_text: String = "".to_string();
    match json_resp {
        Ok(response) => Ok(Box::new(response)),
        Err(e) => Err(LLMError::ResponseFormatError {
            message: format!("Failed to decode API response: {}", e),
            raw_response: resp_text,
        }),
    }
}

// Create an owned OpenAIChatMessage that doesn't borrow from any temporary variables
fn chat_message_to_api_message(chat_msg: ChatMessage) -> OpenAIChatMessage<'static> {
    // For other message types, create an owned OpenAIChatMessage
    OpenAIChatMessage {
        role: match chat_msg.role {
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        },
        tool_call_id: None,
        content: match &chat_msg.message_type {
            MessageType::Text => Some(Right(chat_msg.content.clone())),
            // Image case is handled separately above
            MessageType::Image(_) => unreachable!(),
            MessageType::Pdf(_) => unimplemented!(),
            MessageType::ImageURL(url) => {
                // Clone the URL to create an owned version
                let owned_url = url.clone();
                // Leak the string to get a 'static reference
                let url_str = Box::leak(owned_url.into_boxed_str());
                Some(Left(vec![MessageContent {
                    message_type: Some("image_url"),
                    text: None,
                    image_url: Some(ImageUrlContent { url: url_str }),
                    tool_output: None,
                    tool_call_id: None,
                }]))
            }
            MessageType::ToolUse(_) => None,
            MessageType::ToolResult(_) => None,
        },
        tool_calls: match &chat_msg.message_type {
            MessageType::ToolUse(calls) => {
                let owned_calls: Vec<OpenAIFunctionCall<'static>> = calls
                    .iter()
                    .map(|c| {
                        let owned_id = c.id.clone();
                        let owned_name = c.function.name.clone();
                        let owned_args = c.function.arguments.clone();

                        // Need to leak these strings to create 'static references
                        // This is a deliberate choice to solve the lifetime issue
                        // The small memory leak is acceptable in this context
                        let id_str = Box::leak(owned_id.into_boxed_str());
                        let name_str = Box::leak(owned_name.into_boxed_str());
                        let args_str = Box::leak(owned_args.into_boxed_str());

                        OpenAIFunctionCall {
                            id: id_str,
                            content_type: "function",
                            function: OpenAIFunctionPayload {
                                name: name_str,
                                arguments: args_str,
                            },
                        }
                    })
                    .collect();
                Some(owned_calls)
            }
            _ => None,
        },
    }
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
    if !response.status().is_success() {
        let status = response.status();
        let status_code = status.as_u16();
        let retry_after_secs = if status_code == 429 {
            querymt::plugin::http::parse_retry_after(response.headers())
        } else {
            None
        };

        let clean_message = serde_json::from_slice::<Value>(response.body())
            .ok()
            .and_then(|json| {
                json.pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                format!(
                    "HTTP {}: {}",
                    status_code,
                    String::from_utf8_lossy(response.body())
                )
            });

        return Err(match status_code {
            401 | 403 => LLMError::AuthError(clean_message),
            429 => LLMError::RateLimited {
                message: clean_message,
                retry_after_secs,
            },
            400 => LLMError::InvalidRequest(clean_message),
            500 | 529 => LLMError::ProviderError(format!("Server error: {}", clean_message)),
            _ => LLMError::ProviderError(clean_message),
        });
    }

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
pub struct OpenAIStreamChunk {
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

/// Parse an OpenAI SSE chunk into StreamChunk events
pub fn parse_openai_sse_chunk(
    chunk: &[u8],
    tool_states: &mut HashMap<usize, OpenAIToolUseState>,
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
                stop_reason: "end_turn".to_string(),
            });
            done_emitted = true;
            continue;
        }

        // Parse JSON chunk
        let stream_chunk: OpenAIStreamChunk =
            serde_json::from_str(data).map_err(|e| LLMError::ResponseFormatError {
                message: format!("Failed to parse OpenAI stream chunk: {}", e),
                raw_response: data.to_string(),
            })?;

        // Process each choice
        for choice in &stream_chunk.choices {
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

                // Map finish_reason to unified stop_reason
                let stop_reason = match finish_reason.as_str() {
                    "tool_calls" => "tool_use",
                    "stop" => "end_turn",
                    "length" => "max_tokens",
                    other => other,
                };

                results.push(StreamChunk::Done {
                    stop_reason: stop_reason.to_string(),
                });
                done_emitted = true;
            }
        }

        // Handle usage metadata (typically in final chunk)
        if let Some(usage) = stream_chunk.usage {
            results.push(StreamChunk::Usage(usage.into_usage()));
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use http::Response;
    use querymt::error::LLMError;

    use super::{MultipartForm, openai_parse_list_models};

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
}
