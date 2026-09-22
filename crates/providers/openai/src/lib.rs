//! OpenAI API client implementation for chat and completion functionality.
//!
//! This module provides integration with OpenAI's GPT models through their API.

use http::{Request, Response};
use querymt::{
    HTTPLLMProvider,
    chat::{
        ChatMessage, ChatOutput, StreamChunk, StructuredOutputFormat, Tool, ToolChoice,
        http::{ChatStreamParser, HTTPChatProvider},
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
    stt, tts,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

fn normalize_base_url(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let p = url.path().to_string();
        url.set_path(&(p + "/"));
    }
    url
}

fn deserialize_base_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let url = Url::deserialize(deserializer)?;
    Ok(normalize_base_url(url))
}

/// Authentication type for OpenAI API.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    /// Standard API key authentication (Bearer token).
    #[serde(rename = "api_key")]
    ApiKey,
    /// OAuth token authentication (Bearer token).
    #[serde(rename = "oauth")]
    OAuth,
    /// No authentication. No Authorization header is sent.
    ///
    /// Intended for OpenAI-compatible/self-hosted endpoints.
    #[serde(rename = "none")]
    NoAuth,
}

/// Selects which OpenAI API protocol the provider speaks.
///
/// Omission retains the existing Chat Completions behavior. Responses mode is
/// opt-in and never falls back to Chat Completions after a request is sent.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiMode {
    /// `POST /chat/completions` (default; preserves existing behavior).
    #[default]
    #[serde(rename = "chat_completions")]
    ChatCompletions,
    /// `POST /responses` with stateless local replay.
    #[serde(rename = "responses")]
    Responses,
}

/// Client for interacting with OpenAI's API.
///
/// Provides methods for chat and completion requests using OpenAI's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OpenAI {
    #[serde(default)]
    pub api_key: String,
    /// Optional: Explicitly specify authentication type.
    /// This is only honored when the host is api.openai.com; other hosts always use API keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<AuthType>,
    #[schemars(schema_with = "api::url_schema")]
    #[serde(
        default = "OpenAI::default_base_url",
        deserialize_with = "deserialize_base_url"
    )]
    pub base_url: Url,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default, deserialize_with = "querymt::params::deserialize_system_vec")]
    pub system: Vec<String>,
    pub timeout_seconds: Option<u64>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    /// Embedding parameters
    pub embedding_encoding_format: Option<String>,
    pub embedding_dimensions: Option<u32>,
    pub reasoning_effort: Option<querymt::chat::ReasoningEffort>,
    /// JSON schema for structured output
    pub json_schema: Option<StructuredOutputFormat>,
    /// Which OpenAI API protocol to use. Omitted retains Chat Completions.
    #[serde(default)]
    pub api_mode: ApiMode,
    /// Extra body fields to include in the API request (e.g. `store`, `promptCacheKey`).
    /// These are passed through as-is via `#[serde(flatten)]` in the request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, Value>>,
}

impl OpenAI {
    fn default_base_url() -> Url {
        Url::parse("https://api.openai.com/v1/").unwrap()
    }
}

pub mod api;

impl api::OpenAIProviderConfig for OpenAI {
    fn api_key(&self) -> &str {
        &self.api_key
    }

    fn auth_type(&self) -> Option<&AuthType> {
        self.auth_type.as_ref()
    }

    fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn max_tokens(&self) -> Option<&u32> {
        self.max_tokens.as_ref()
    }

    fn temperature(&self) -> Option<&f32> {
        self.temperature.as_ref()
    }

    fn system(&self) -> &[String] {
        &self.system
    }

    fn timeout_seconds(&self) -> Option<&u64> {
        self.timeout_seconds.as_ref()
    }

    fn stream(&self) -> Option<&bool> {
        self.stream.as_ref()
    }

    fn top_p(&self) -> Option<&f32> {
        self.top_p.as_ref()
    }

    fn top_k(&self) -> Option<&u32> {
        self.top_k.as_ref()
    }

    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }

    fn tool_choice(&self) -> Option<&ToolChoice> {
        self.tool_choice.as_ref()
    }

    fn embedding_encoding_format(&self) -> Option<&str> {
        self.embedding_encoding_format.as_deref()
    }

    fn embedding_dimensions(&self) -> Option<&u32> {
        self.embedding_dimensions.as_ref()
    }

    fn reasoning_effort(&self) -> Option<querymt::chat::ReasoningEffort> {
        self.reasoning_effort
    }

    fn json_schema(&self) -> Option<&StructuredOutputFormat> {
        self.json_schema.as_ref()
    }

    fn api_mode(&self) -> ApiMode {
        self.api_mode
    }

    fn extra_body(&self) -> Option<serde_json::Map<String, Value>> {
        self.extra_body.clone()
    }
}

impl HTTPChatProvider for OpenAI {
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        api::classify_openai_http_error(response)
    }

    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        match self.api_mode {
            // Default path: behavior is unchanged and request snapshots are stable.
            ApiMode::ChatCompletions => api::openai_chat_request(self, messages, tools),
            // Opt-in only. This never falls back to Chat Completions after sending.
            ApiMode::Responses => api::openai_responses_request(self, messages, tools),
        }
    }

    fn chat_stream_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut cfg = self.clone();
        cfg.stream = Some(true);
        match cfg.api_mode {
            ApiMode::ChatCompletions => api::openai_chat_request(&cfg, messages, tools),
            ApiMode::Responses => api::openai_responses_request(&cfg, messages, tools),
        }
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<ChatOutput, LLMError> {
        match self.api_mode {
            ApiMode::ChatCompletions => api::openai_parse_chat(self, response),
            ApiMode::Responses => api::openai_parse_responses(self, response, None),
        }
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        match self.api_mode {
            ApiMode::ChatCompletions => Ok(Box::new(OpenAIStreamParser::default())),
            ApiMode::Responses => Ok(Box::new(OpenAIResponsesStreamParser::default())),
        }
    }
}

#[derive(Default)]
struct OpenAIStreamParser {
    tool_states: HashMap<usize, api::OpenAIToolUseState>,
}

impl ChatStreamParser for OpenAIStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        api::parse_openai_sse_chunk(chunk, &mut self.tool_states)
    }
}

/// Request-local semantic parser for the Responses API.
///
/// State (response identity, terminal status, whether a local call was seen) is
/// scoped to this parser instance, so concurrent streams never share it.
#[derive(Default)]
struct OpenAIResponsesStreamParser {
    state: api::OpenAIResponsesStreamState,
}

impl ChatStreamParser for OpenAIResponsesStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        api::parse_openai_responses_sse_chunk(chunk, &mut self.state)
    }
}

impl HTTPEmbeddingProvider for OpenAI {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        api::openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        api::openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for OpenAI {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for OpenAI {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }

    fn stt_request(&self, req: &stt::SttRequest) -> Result<Request<Vec<u8>>, LLMError> {
        api::openai_stt_request(self, req)
    }

    fn parse_stt(&self, resp: Response<Vec<u8>>) -> Result<stt::SttResponse, LLMError> {
        api::openai_parse_stt(self, resp)
    }

    fn tts_request(&self, req: &tts::TtsRequest) -> Result<Request<Vec<u8>>, LLMError> {
        api::openai_tts_request(self, req)
    }

    fn parse_tts(&self, resp: Response<Vec<u8>>) -> Result<tts::TtsResponse, LLMError> {
        api::openai_parse_tts(self, resp)
    }
}

struct OpenAIFactory;
impl HTTPLLMProviderFactory for OpenAIFactory {
    fn name(&self) -> &str {
        "openai"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("OPENAI_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => normalize_base_url(Url::parse(base_url_str)?),
            None => normalize_base_url(OpenAI::default_base_url()),
        };
        api::openai_list_models_request(&base_url, &cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        api::openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(OpenAI);
        // Extract the schema object and turn it into a JSON string
        serde_json::to_string(&schema).expect("OpenAI JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let mut provider: OpenAI = serde_json::from_str(cfg)?;
        provider.base_url = normalize_base_url(provider.base_url);
        Ok(Box::new(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiMode, OpenAI};
    use querymt::{
        chat::{
            ChatInputPart, ChatMessage, ChatRole, MediaKind, MediaPart, MediaSource, StreamChunk,
            Tool, ToolChoice, ToolResult, ToolResultPart,
            http::{ChatStreamParser, HTTPChatProvider},
        },
        error::LLMError,
    };
    use serde_json::Value;

    #[test]
    fn api_mode_defaults_to_chat_completions_when_omitted() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        assert_eq!(provider.api_mode, ApiMode::ChatCompletions);
    }

    #[test]
    fn api_mode_accepts_explicit_responses_selection() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        assert_eq!(provider.api_mode, ApiMode::Responses);
    }

    #[test]
    fn config_schema_exposes_api_mode_with_chat_completions_default() {
        let schema = querymt::plugin::HTTPLLMProviderFactory::config_schema(&super::OpenAIFactory);
        let schema: Value = serde_json::from_str(&schema).expect("schema is valid json");
        let api_mode = schema
            .get("properties")
            .and_then(|p| p.get("api_mode"))
            .expect("api_mode is part of the config schema");
        let default = api_mode.get("default").expect("api_mode carries a default");
        assert_eq!(default, &Value::String("chat_completions".to_string()));
        // Omission must not require callers to supply the field.
        let required = schema
            .get("required")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            !required.iter().any(|key| key == "api_mode"),
            "api_mode must not be required"
        );
    }

    #[test]
    fn default_mode_request_keeps_chat_completions_endpoint_and_body() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "system": ["be terse"]
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];

        let req = provider
            .chat_request(&messages, None)
            .expect("default request should build");

        assert!(
            req.uri().path().ends_with("/chat/completions"),
            "omitted selection must use the existing endpoint, got {}",
            req.uri()
        );
        let body: Value = serde_json::from_slice(req.body()).expect("body is valid json");
        assert!(body.get("messages").is_some(), "uses the messages array");
        assert!(
            body.get("input").is_none(),
            "must not emit Responses-style input by default"
        );
        assert!(
            body.get("instructions").is_none(),
            "must not emit Responses-style instructions by default"
        );
        assert!(
            body.get("store").is_none(),
            "must not emit Responses store field by default"
        );
    }

    #[test]
    fn responses_mode_selects_responses_endpoint_and_body() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses",
            "system": ["be terse"]
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];

        let req = provider
            .chat_request(&messages, None)
            .expect("responses request should build");

        assert!(
            req.uri().path().ends_with("/responses"),
            "explicit selection must use the Responses endpoint, got {}",
            req.uri()
        );
        let body: Value = serde_json::from_slice(req.body()).expect("body is valid json");
        assert!(body.get("input").is_some(), "uses the ordered input array");
        assert_eq!(body.get("store"), Some(&Value::Bool(false)));
        assert!(
            body.get("messages").is_none(),
            "must not emit Chat Completions messages"
        );
        assert_eq!(
            body.get("instructions"),
            Some(&Value::String("be terse".to_string()))
        );
    }

    #[test]
    fn responses_tool_choice_uses_named_shape() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        let tools = vec![Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: "read_file".to_string(),
                description: "read a file".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                strict: None,
            },
        }];

        let mut provider = provider;
        provider.tool_choice = Some(ToolChoice::Tool("read_file".to_string()));

        let req = provider
            .chat_request(&messages, Some(&tools))
            .expect("responses request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body is valid json");

        // Flattened definition with explicit non-strict default.
        let tool = &body["tools"][0];
        assert_eq!(tool["name"], Value::String("read_file".to_string()));
        assert_eq!(tool["strict"], Value::Bool(false));
        assert!(tool.get("function").is_none(), "definition is flattened");

        // Protocol-correct named tool choice.
        assert_eq!(
            body["tool_choice"],
            serde_json::json!({"type": "function", "name": "read_file"})
        );
    }

    #[test]
    fn api_mode_roundtrips_through_serde() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let serialized = serde_json::to_value(&provider).unwrap();
        assert_eq!(
            serialized.get("api_mode"),
            Some(&Value::String("responses".to_string()))
        );
        let reparsed: OpenAI = serde_json::from_value(serialized).unwrap();
        assert_eq!(reparsed.api_mode, ApiMode::Responses);
    }

    #[test]
    fn responses_input_preserves_item_order() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();

        let messages = vec![
            ChatMessage::user().text("read a.txt").build(),
            ChatMessage::from(querymt::chat::ChatOutput {
                items: vec![
                    querymt::chat::ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                        id: None,
                        role: ChatRole::Assistant,
                        phase: None,
                        status: None,
                        parts: vec![querymt::chat::ChatMessagePart::Text {
                            text: "on it".to_string(),
                            annotations: Vec::new(),
                            extensions: querymt::chat::Extensions::new(),
                        }],
                        extensions: querymt::chat::Extensions::new(),
                    }),
                    querymt::chat::ChatOutputItem::FunctionCall(
                        querymt::chat::ChatFunctionCallItem {
                            item_id: Some("item_1".to_string()),
                            call_id: "call_1".to_string(),
                            name: "read_file".to_string(),
                            arguments: "{\"path\":\"a.txt\"}".to_string(),
                            status: None,
                            extensions: querymt::chat::Extensions::new(),
                        },
                    ),
                ],
                ..querymt::chat::ChatOutput::default()
            }),
            ChatMessage::user()
                .part(querymt::chat::ChatInputPart::tool_result(
                    querymt::chat::ToolResult::text("call_1", "file body"),
                ))
                .text("summarize")
                .build(),
        ];

        let req = provider
            .chat_request(&messages, None)
            .expect("responses request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body is valid json");
        let input = body["input"].as_array().expect("input is an array");

        let types: Vec<&str> = input
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            vec![
                "message",
                "message",
                "function_call",
                "function_call_output",
                "message"
            ],
            "input items must retain chronological order: {types:?}"
        );

        // Correlation uses call_id, not output item id.
        let call = &input[2];
        assert_eq!(call["call_id"], Value::String("call_1".to_string()));
        assert_eq!(call["name"], Value::String("read_file".to_string()));
        assert_eq!(
            call["arguments"],
            Value::String(r#"{"path":"a.txt"}"#.to_string())
        );
        assert_eq!(input[3]["call_id"], Value::String("call_1".to_string()));
    }

    #[test]
    fn responses_request_is_stateless_and_requests_encrypted_reasoning() {
        let provider = responses_provider();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        let req = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(req.body()).unwrap();

        assert_eq!(
            body.get("store"),
            Some(&Value::Bool(false)),
            "remote storage must be disabled"
        );
        assert_eq!(
            body.get("include"),
            Some(&serde_json::json!(["reasoning.encrypted_content"])),
            "encrypted reasoning must be requested for local replay"
        );
        assert!(
            body.get("previous_response_id").is_none(),
            "stateful continuation must not be referenced"
        );
        assert!(
            body.get("conversation").is_none(),
            "conversation references must not be emitted"
        );
    }

    #[test]
    fn responses_rejects_retention_and_continuation_overrides() {
        for reserved in ["store", "previous_response_id", "conversation", "include"] {
            let cfg = serde_json::json!({
                "api_key": "test-key",
                "model": "gpt-4o-mini",
                "api_mode": "responses",
                "extra_body": { reserved: true }
            });
            let provider: OpenAI = serde_json::from_value(cfg).unwrap();
            let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
                "hello".to_string(),
            )])];
            let error = provider
                .chat_request(&messages, None)
                .expect_err("reserved override must fail");
            assert!(
                matches!(error, LLMError::InvalidRequest(ref msg) if msg.contains(reserved)),
                "expected conflict error for '{reserved}', got {error:?}"
            );
        }
    }

    #[test]
    fn responses_rejects_camel_case_retention_alias_on_compatible_host() {
        // Non-OpenAI hosts skip key normalization, so the alias must be caught
        // in its raw spelling too.
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "base_url": "https://compatible.example.invalid/v1",
            "model": "gpt-4o-mini",
            "api_mode": "responses",
            "extra_body": { "previousResponseId": "resp_123" }
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        let error = provider
            .chat_request(&messages, None)
            .expect_err("camelCase retention alias must fail");
        assert!(
            matches!(error, LLMError::InvalidRequest(ref msg) if msg.contains("previousResponseId")),
            "expected alias conflict error, got {error:?}"
        );
    }

    #[test]
    fn responses_allows_unrelated_extra_body_fields() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses",
            "extra_body": { "metadata": {"trace": "abc"} }
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        let req = provider
            .chat_request(&messages, None)
            .expect("unrelated passthrough must be allowed");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        assert_eq!(body["metadata"]["trace"], Value::String("abc".to_string()));
        assert_eq!(body["store"], Value::Bool(false));
    }

    #[test]
    fn responses_maps_request_controls_to_wire_semantics() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses",
            "system": ["be terse", "answer only"],
            "max_tokens": 512,
            "reasoning_effort": "high",
            "json_schema": {
                "name": "Answer",
                "description": "final answer",
                "schema": {"type": "object", "properties": {"value": {"type": "string"}}},
                "strict": true
            }
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "answer me".to_string(),
        )])];

        let req = provider
            .chat_request(&messages, None)
            .expect("request controls should map");
        let body: Value = serde_json::from_slice(req.body()).unwrap();

        // System config -> instructions.
        assert_eq!(
            body["instructions"],
            Value::String("be terse\n\nanswer only".to_string())
        );
        // max_tokens -> max_output_tokens.
        assert_eq!(body["max_output_tokens"], serde_json::json!(512));
        assert!(
            body.get("max_tokens").is_none(),
            "Chat Completions max_tokens must not leak into Responses"
        );
        // JSON schema -> text.format.
        assert_eq!(
            body["text"]["format"]["type"],
            Value::String("json_schema".to_string())
        );
        assert_eq!(
            body["text"]["format"]["name"],
            Value::String("Answer".to_string())
        );
        assert_eq!(body["text"]["format"]["strict"], Value::Bool(true));
        assert!(body["text"]["format"]["schema"].is_object());
        assert!(
            body.get("response_format").is_none(),
            "Chat Completions response_format must not leak into Responses"
        );
        // reasoning effort -> reasoning.effort.
        assert_eq!(
            body["reasoning"]["effort"],
            Value::String("high".to_string())
        );
    }

    #[test]
    fn responses_rejects_unsupported_sampling_parameter() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses",
            "top_k": 40
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];

        let error = provider
            .chat_request(&messages, None)
            .expect_err("top_k has no Responses equivalent");
        assert!(
            matches!(error, LLMError::InvalidRequest(ref msg) if msg.contains("top_k")),
            "expected explicit unsupported-parameter error, got {error:?}"
        );
    }

    #[test]
    fn responses_omits_controls_that_are_not_configured() {
        let provider = responses_provider();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        let req = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(req.body()).unwrap();

        assert!(body.get("instructions").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("text").is_none());
        assert!(body.get("reasoning").is_none());
    }

    fn responses_tool(strict: Option<bool>, parameters: serde_json::Value) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: querymt::chat::FunctionTool {
                name: "lookup".to_string(),
                description: "look something up".to_string(),
                parameters,
                strict,
            },
        }
    }

    #[test]
    fn responses_omitted_strictness_serializes_as_false() {
        let provider = responses_provider();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        // A schema with an optional property is fine when strictness is omitted.
        let tools = vec![responses_tool(
            None,
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }),
        )];

        let req = provider
            .chat_request(&messages, Some(&tools))
            .expect("non-strict tool must build");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        assert_eq!(body["tools"][0]["strict"], Value::Bool(false));
    }

    #[test]
    fn responses_rejects_incompatible_strict_schema_without_altering_optionality() {
        let provider = responses_provider();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        // `query` is optional and additionalProperties is unset, so strict mode
        // cannot be honored without changing the schema's meaning.
        let tools = vec![responses_tool(
            Some(true),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["query"]
            }),
        )];

        let error = provider
            .chat_request(&messages, Some(&tools))
            .expect_err("incompatible strict schema must fail");
        assert!(
            matches!(error, LLMError::InvalidRequest(ref msg) if msg.contains("lookup")),
            "expected strict schema error, got {error:?}"
        );
    }

    #[test]
    fn responses_accepts_compatible_strict_schema() {
        let provider = responses_provider();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        let tools = vec![responses_tool(
            Some(true),
            serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
        )];

        let req = provider
            .chat_request(&messages, Some(&tools))
            .expect("compatible strict schema must build");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        assert_eq!(body["tools"][0]["strict"], Value::Bool(true));
    }

    #[test]
    fn chat_completions_preserves_omitted_strictness() {
        // Chat Completions must not gain a strict=false default.
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let messages = vec![ChatMessage::from_user_parts(vec![ChatInputPart::text(
            "hello".to_string(),
        )])];
        let tools = vec![responses_tool(
            None,
            serde_json::json!({"type": "object", "properties": {}}),
        )];

        let req = provider
            .chat_request(&messages, Some(&tools))
            .expect("chat completions tool must build");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        assert!(
            body["tools"][0]["function"].get("strict").is_none(),
            "omitted strictness must stay omitted in Chat Completions"
        );
    }

    #[test]
    fn responses_rich_function_output_preserves_part_order_and_call_id() {
        let provider = responses_provider();

        // Assistant turn with a function call whose call id differs from its item id.
        let call = querymt::chat::ChatFunctionCallItem {
            item_id: Some("fc_item_7".to_string()),
            call_id: "call_7".to_string(),
            name: "render".to_string(),
            arguments: r#"{"target":"report"}"#.to_string(),
            status: None,
            extensions: querymt::chat::Extensions::new(),
        };
        let output = querymt::chat::ChatOutput {
            items: vec![querymt::chat::ChatOutputItem::FunctionCall(call)],
            ..querymt::chat::ChatOutput::default()
        };
        let assistant = ChatMessage::from_assistant_output(output);

        let result = ChatMessage::from_user_parts(vec![ChatInputPart::tool_result(ToolResult {
            call_id: "call_7".to_string(),
            name: Some("render".to_string()),
            is_error: false,
            parts: vec![
                ToolResultPart::text("here is the result"),
                attachment_part(MediaKind::Image, "image/png".to_string(), vec![1, 2, 3]),
                attachment_part(
                    MediaKind::Document,
                    "application/pdf".to_string(),
                    vec![4, 5, 6],
                ),
            ],
            extensions: Default::default(),
        })]);

        let req = provider
            .chat_request(&[assistant, result], None)
            .expect("rich function output should build");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        let input = body["input"].as_array().unwrap();

        // Call correlates by call_id (not item_id).
        assert_eq!(input[0]["type"], Value::String("function_call".to_string()));
        assert_eq!(input[0]["call_id"], Value::String("call_7".to_string()));

        let function_output = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("function output present");
        assert_eq!(
            function_output["call_id"],
            Value::String("call_7".to_string())
        );

        // Ordered rich parts: text, image, file.
        let parts = function_output["output"].as_array().unwrap();
        let kinds: Vec<&str> = parts.iter().map(|p| p["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["output_text", "input_image", "input_file"]);
        assert_eq!(
            parts[0]["text"],
            Value::String("here is the result".to_string())
        );
        assert!(
            parts[1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(
            parts[2]["file_data"]
                .as_str()
                .unwrap()
                .starts_with("data:application/pdf;base64,")
        );
    }

    #[test]
    fn responses_text_only_function_output_uses_string_form() {
        let provider = responses_provider();
        let result = ChatMessage::from_user_parts(vec![ChatInputPart::tool_result(ToolResult {
            call_id: "call_1".to_string(),
            name: None,
            is_error: false,
            parts: vec![ToolResultPart::text("plain result")],
            extensions: Default::default(),
        })]);

        let req = provider
            .chat_request(&[result], None)
            .expect("text result should build");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        assert_eq!(
            body["input"][0]["output"],
            Value::String("plain result".to_string())
        );
    }

    #[test]
    fn responses_rejects_unsupported_function_output_media_without_placeholder() {
        let provider = responses_provider();
        let result = ChatMessage::from_user_parts(vec![ChatInputPart::tool_result(ToolResult {
            call_id: "call_1".to_string(),
            name: None,
            is_error: false,
            parts: vec![attachment_part(
                MediaKind::Audio,
                "audio/wav".to_string(),
                vec![1, 2, 3],
            )],
            extensions: Default::default(),
        })]);

        let error = provider
            .chat_request(&[result], None)
            .expect_err("unsupported output media must fail");
        assert!(
            matches!(error, LLMError::InvalidRequest(ref msg)
                if msg.contains("unsupported") && msg.contains("Audio")),
            "expected explicit unsupported-media error, got {error:?}"
        );
    }

    fn responses_json_response(body: Value) -> http::Response<Vec<u8>> {
        http::Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body).unwrap())
            .unwrap()
    }

    #[test]
    fn responses_normalizes_completed_function_response_with_exclusive_usage() {
        let provider = responses_provider();
        let body = serde_json::json!({
            "id": "resp_1",
            "status": "completed",
            "model": "gpt-4o-mini",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "thinking"}],
                    "encrypted_content": "enc"
                },
                {
                    "type": "message",
                    "id": "msg_1",
                    "content": [
                        {"type": "output_text", "text": "calling", "annotations": [
                            {"type": "citation", "url": "https://example.invalid"}
                        ]}
                    ]
                },
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a\"}"},
                {"type": "function_call", "id": "fc_2", "call_id": "call_2", "name": "read", "arguments": "{\"path\":\"b\"}"}
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "input_tokens_details": {"cached_tokens": 30},
                "output_tokens_details": {"reasoning_tokens": 20}
            }
        });

        let response = provider
            .parse_chat(responses_json_response(body))
            .expect("completed response should parse");

        let output = &response;
        assert_eq!(output.items.len(), 4);
        assert!(matches!(
            output.items[0],
            querymt::chat::ChatOutputItem::Reasoning(_)
        ));
        assert!(matches!(
            output.items[1],
            querymt::chat::ChatOutputItem::Message(_)
        ));
        assert_eq!(output.tool_calls().unwrap().len(), 2);
        assert_eq!(
            response.finish_reason,
            Some(querymt::chat::FinishReason::ToolCalls)
        );

        // Cached and reasoning tokens are counted once, not double-counted.
        let usage = response.usage.as_ref().unwrap();
        assert_eq!(usage.cache_read, 30);
        assert_eq!(usage.reasoning_tokens, 20);
        assert_eq!(usage.input_tokens, 70);
        assert_eq!(usage.output_tokens, 30);

        // Annotations survive normalization.
        let querymt::chat::ChatOutputItem::Message(message) = &output.items[1] else {
            panic!("expected message item");
        };
        let querymt::chat::ChatMessagePart::Text { annotations, .. } = &message.parts[0] else {
            panic!("expected text part");
        };
        assert_eq!(annotations.len(), 1);
        assert_eq!(annotations[0].annotation_type, "citation");
    }

    #[test]
    fn responses_normalizes_refusal_and_encrypted_only_reasoning() {
        let provider = responses_provider();
        let body = serde_json::json!({
            "id": "resp_2",
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_2",
                    "summary": [],
                    "encrypted_content": "encrypted-only"
                },
                {
                    "type": "message",
                    "id": "msg_2",
                    "content": [{"type": "refusal", "refusal": "cannot comply"}]
                }
            ]
        });

        let response = provider
            .parse_chat(responses_json_response(body))
            .expect("response should parse");
        let output = &response;

        let querymt::chat::ChatOutputItem::Reasoning(reasoning) = &output.items[0] else {
            panic!("expected reasoning item");
        };
        assert!(reasoning.summary.is_empty());
        assert_eq!(
            reasoning.encrypted_content.as_deref(),
            Some("encrypted-only")
        );

        let querymt::chat::ChatOutputItem::Message(message) = &output.items[1] else {
            panic!("expected message item");
        };
        let querymt::chat::ChatMessagePart::Refusal { refusal, .. } = &message.parts[0] else {
            panic!("expected refusal part");
        };
        assert_eq!(refusal, "cannot comply");

        assert_eq!(
            response.finish_reason,
            Some(querymt::chat::FinishReason::Stop)
        );
    }

    #[test]
    fn responses_incomplete_retains_partial_output_and_cause() {
        let provider = responses_provider();
        let body = serde_json::json!({
            "id": "resp_3",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [
                {"type": "message", "id": "msg_3", "content": [{"type": "output_text", "text": "partial"}]}
            ]
        });

        let response = provider
            .parse_chat(responses_json_response(body))
            .expect("incomplete response should parse");
        let output = &response;

        assert_eq!(
            output.status,
            Some(querymt::chat::ChatOutputStatus::Incomplete)
        );
        assert_eq!(response.text().as_deref(), Some("partial"));
        assert_eq!(
            response.finish_reason,
            Some(querymt::chat::FinishReason::Length),
            "incomplete max_output_tokens is a length terminal, not a failure"
        );
    }

    #[test]
    fn responses_retains_unknown_builtin_items_as_opaque() {
        let provider = responses_provider();
        let body = serde_json::json!({
            "id": "resp_4",
            "status": "completed",
            "output": [
                {
                    "type": "image_generation_call",
                    "id": "ig_1",
                    "result": "base64-bytes",
                    "status": "completed"
                }
            ]
        });

        let response = provider
            .parse_chat(responses_json_response(body))
            .expect("unknown item should parse");
        let output = &response;

        let querymt::chat::ChatOutputItem::Opaque(opaque) = &output.items[0] else {
            panic!("expected opaque item");
        };
        assert_eq!(opaque.original_type, "image_generation_call");
        assert!(
            opaque.payload.get("result").is_some(),
            "opaque payload retained complete"
        );
        assert!(
            response.tool_calls().is_none(),
            "opaque items must not enter the local function executor"
        );
        assert!(
            response.text().is_none(),
            "opaque items must not be fabricated into message content"
        );
    }

    #[test]
    fn responses_maps_failed_status_to_error() {
        let provider = responses_provider();
        let body = serde_json::json!({
            "id": "resp_5",
            "status": "failed",
            "error": {"code": "server_error", "message": "boom"},
            "output": []
        });

        let error = provider
            .parse_chat(responses_json_response(body))
            .expect_err("failed status with error must surface provider failure");
        assert!(
            matches!(error, LLMError::ProviderResponseError(_)),
            "expected classified provider failure, got {error:?}"
        );
    }

    fn sse(data: &str) -> Vec<u8> {
        format!("data: {data}\n\n").into_bytes()
    }

    #[test]
    fn responses_stream_parser_emits_semantic_events_with_complete_snapshots() {
        use querymt::chat::{ChatOutputItem, ChatOutputStatus, StructuredStreamEvent};

        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();

        let mut events = Vec::new();
        events.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-4o-mini"}}"#,
                ))
                .unwrap(),
        );
        events.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","content":[]}}"#,
                ))
                .unwrap(),
        );
        events.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hello"}"#,
                ))
                .unwrap(),
        );
        // Complete snapshot replaces provisional deltas in the accumulator.
        events.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","status":"completed","content":[{"type":"output_text","text":"hello world"}]}}"#,
                ))
                .unwrap(),
        );
        events.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":4}}}"#,
                ))
                .unwrap(),
        );

        // First event establishes structured metadata with provenance.
        assert!(matches!(
            &events[0],
            StreamChunk::Structured(StructuredStreamEvent::ResponseMetadata {
                response_id: Some(id),
                status: Some(ChatOutputStatus::InProgress),
                provenance: Some(_),
                ..
            }) if id == "resp_1"
        ));
        assert!(matches!(
            &events[1],
            StreamChunk::Structured(StructuredStreamEvent::ItemStarted {
                output_index: 0,
                ..
            })
        ));
        assert!(matches!(
            &events[2],
            StreamChunk::Structured(StructuredStreamEvent::MessagePartDelta {
                output_index: 0,
                content_index: 0,
                ..
            })
        ));
        let StreamChunk::Structured(StructuredStreamEvent::ItemCompleted { item, .. }) = &events[3]
        else {
            panic!("expected item completion snapshot");
        };
        let ChatOutputItem::Message(message) = item else {
            panic!("expected message snapshot");
        };
        assert!(matches!(
            &message.parts[0],
            querymt::chat::ChatMessagePart::Text { text, .. } if text == "hello world"
        ));
        assert!(matches!(
            &events[4],
            StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                status: ChatOutputStatus::Completed,
                finish_reason: Some(querymt::chat::FinishReason::Stop),
                ..
            })
        ));
    }

    #[test]
    fn responses_message_role_does_not_collide_in_serialized_stream_chunk() {
        use querymt::chat::{ChatOutputItem, StructuredStreamEvent};

        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let events = parser
            .parse_chunk(&sse(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","phase":"final_answer","custom_field":{"retained":true},"content":[{"type":"output_text","text":"Madrid"}]}}"#,
            ))
            .unwrap();
        let chunk = events.into_iter().next().expect("item completion event");

        let encoded = serde_json::to_vec(&chunk)
            .expect("stream chunk serializes without duplicate canonical fields");
        let decoded: querymt::chat::StreamChunk =
            serde_json::from_slice(&encoded).expect("transport can deserialize the message chunk");

        let querymt::chat::StreamChunk::Structured(StructuredStreamEvent::ItemCompleted {
            item: ChatOutputItem::Message(message),
            ..
        }) = decoded
        else {
            panic!("expected completed message item");
        };
        assert_eq!(message.role, querymt::chat::ChatRole::Assistant);
        assert_eq!(message.phase.as_deref(), Some("final_answer"));
        assert_eq!(message.extensions["custom_field"]["retained"], true);
        assert!(!message.extensions.contains_key("role"));
        assert!(!message.extensions.contains_key("phase"));
    }

    #[test]
    fn responses_stream_ignores_framing_only_done_marker() {
        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let events = parser.parse_chunk(&sse("[DONE]")).unwrap();
        assert!(
            events.is_empty(),
            "[DONE] is transport framing, not a semantic terminal"
        );
    }

    #[test]
    fn responses_part_then_delta_accumulates_end_to_end() {
        use querymt::chat::{ChatOutputStatus, ChatStreamAccumulator};

        // The real SSE sequence for ordinary assistant text: the message item
        // is added with no content, then `content_part.added` declares the
        // indexed part, then text deltas arrive. Feeding the emitted events
        // through the shared accumulator must succeed and produce the text.
        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut accumulator = ChatStreamAccumulator::new();

        let mut feed = |parser: &mut Box<dyn ChatStreamParser>,
                        accumulator: &mut ChatStreamAccumulator,
                        payload: &str| {
            for chunk in parser.parse_chunk(&sse(payload)).unwrap() {
                accumulator.push(&chunk).expect("accumulator accepts event");
            }
        };

        feed(
            &mut parser,
            &mut accumulator,
            r#"{"type":"response.created","response":{"id":"resp_e2e","model":"gpt-4o-mini"}}"#,
        );
        feed(
            &mut parser,
            &mut accumulator,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_e2e","content":[]}}"#,
        );
        feed(
            &mut parser,
            &mut accumulator,
            r#"{"type":"response.content_part.added","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}"#,
        );
        feed(
            &mut parser,
            &mut accumulator,
            r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hello "}"#,
        );
        feed(
            &mut parser,
            &mut accumulator,
            r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"world"}"#,
        );
        feed(
            &mut parser,
            &mut accumulator,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_e2e","status":"completed","content":[{"type":"output_text","text":"hello world"}]}}"#,
        );
        feed(
            &mut parser,
            &mut accumulator,
            r#"{"type":"response.completed","response":{"id":"resp_e2e","status":"completed"}}"#,
        );

        let output = accumulator
            .finish_success()
            .expect("accumulation completes");
        assert_eq!(output.status, Some(ChatOutputStatus::Completed));
        assert_eq!(output.text().as_deref(), Some("hello world"));
    }

    #[test]
    fn responses_text_delta_without_part_added_still_accumulates() {
        use querymt::chat::{ChatOutputStatus, ChatStreamAccumulator};

        // Defensive: a provider may omit `content_part.added`; the accumulator
        // must create the indexed part on demand instead of failing.
        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut accumulator = ChatStreamAccumulator::new();

        for payload in [
            r#"{"type":"response.created","response":{"id":"resp_def","model":"gpt-4o-mini"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_def","content":[]}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"hi"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_def","status":"completed","content":[{"type":"output_text","text":"hi"}]}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_def","status":"completed"}}"#,
        ] {
            for chunk in parser.parse_chunk(&sse(payload)).unwrap() {
                accumulator.push(&chunk).expect("accumulator accepts event");
            }
        }

        let output = accumulator
            .finish_success()
            .expect("accumulation completes");
        assert_eq!(output.status, Some(ChatOutputStatus::Completed));
        assert_eq!(output.text().as_deref(), Some("hi"));
    }

    #[test]
    fn responses_reasoning_summary_part_then_delta_accumulates_end_to_end() {
        use querymt::chat::{ChatOutputStatus, ChatStreamAccumulator};

        // Reasoning summaries have the same part/delta ordering: the summary
        // part is declared by `reasoning_summary_part.added` before deltas.
        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut accumulator = ChatStreamAccumulator::new();

        for payload in [
            r#"{"type":"response.created","response":{"id":"resp_r","model":"gpt-5"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
            r#"{"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"thinking"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"thinking"}]}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_r","status":"completed"}}"#,
        ] {
            for chunk in parser.parse_chunk(&sse(payload)).unwrap() {
                accumulator.push(&chunk).expect("accumulator accepts event");
            }
        }

        let output = accumulator
            .finish_success()
            .expect("accumulation completes");
        assert_eq!(output.status, Some(ChatOutputStatus::Completed));
        assert_eq!(output.thinking().as_deref(), Some("thinking"));
    }

    #[test]
    fn responses_stream_completion_with_call_indicates_tool_calls() {
        use querymt::chat::{ChatOutputStatus, StructuredStreamEvent};

        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        parser
            .parse_chunk(&sse(
                r#"{"type":"response.created","response":{"id":"resp_2"}}"#,
            ))
            .unwrap();
        parser
            .parse_chunk(&sse(
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""}}"#,
            ))
            .unwrap();
        parser
            .parse_chunk(&sse(
                r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}"#,
            ))
            .unwrap();
        parser
            .parse_chunk(&sse(
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","status":"completed","arguments":"{\"path\":\"a.txt\"}"}}"#,
            ))
            .unwrap();
        let events = parser
            .parse_chunk(&sse(
                r#"{"type":"response.completed","response":{"id":"resp_2","status":"completed"}}"#,
            ))
            .unwrap();

        assert!(matches!(
            &events[0],
            StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                status: ChatOutputStatus::Completed,
                finish_reason: Some(querymt::chat::FinishReason::ToolCalls),
                ..
            })
        ));
    }

    #[test]
    fn responses_stream_incomplete_emits_retained_terminal() {
        use querymt::chat::{ChatOutputStatus, StructuredStreamEvent};

        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let events = parser
            .parse_chunk(&sse(
                r#"{"type":"response.incomplete","response":{"id":"resp_3","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
            ))
            .unwrap();

        let StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
            status,
            finish_reason,
            detail,
            ..
        }) = &events[0]
        else {
            panic!("expected terminal event");
        };
        assert_eq!(*status, ChatOutputStatus::Incomplete);
        assert_eq!(*finish_reason, Some(querymt::chat::FinishReason::Length));
        assert_eq!(detail.as_deref(), Some("max_output_tokens"));
    }

    #[test]
    fn responses_stream_failed_surfaces_provider_failure() {
        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let error = parser
            .parse_chunk(&sse(
                r#"{"type":"response.failed","response":{"id":"resp_4","status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
            ))
            .expect_err("failed event must be an error");
        assert!(
            matches!(error, LLMError::ProviderResponseError(_)),
            "expected classified provider failure, got {error:?}"
        );
    }

    #[test]
    fn responses_stream_parsers_are_isolated_per_stream() {
        use querymt::chat::StructuredStreamEvent;

        let provider = responses_provider();
        let mut parser_a = provider.chat_stream_parser().unwrap();
        let mut parser_b = provider.chat_stream_parser().unwrap();

        // Parser A establishes a response and terminal.
        parser_a
            .parse_chunk(&sse(
                r#"{"type":"response.created","response":{"id":"resp_a"}}"#,
            ))
            .unwrap();
        let a_events = parser_a
            .parse_chunk(&sse(
                r#"{"type":"response.completed","response":{"id":"resp_a","status":"completed"}}"#,
            ))
            .unwrap();
        assert!(matches!(
            a_events.last(),
            Some(StreamChunk::Structured(
                StructuredStreamEvent::ResponseTerminal { .. }
            ))
        ));

        // Parser B is unaffected: it still needs and accepts its own metadata.
        let b_events = parser_b
            .parse_chunk(&sse(
                r#"{"type":"response.created","response":{"id":"resp_b"}}"#,
            ))
            .unwrap();
        assert!(
            matches!(
                b_events.first(),
                Some(StreamChunk::Structured(
                    StructuredStreamEvent::ResponseMetadata { .. }
                ))
            ),
            "independent stream must start its own attempt"
        );
    }

    fn tagged_assistant_turn(parts: Vec<querymt::chat::ChatMessagePart>) -> ChatMessage {
        let output = querymt::chat::ChatOutput {
            items: vec![querymt::chat::ChatOutputItem::Message(
                querymt::chat::ChatMessageItem {
                    id: Some("msg_media".to_string()),
                    role: ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts,
                    extensions: querymt::chat::Extensions::new(),
                },
            )],
            ..querymt::chat::ChatOutput::default()
        };
        ChatMessage::from_assistant_output(output)
    }

    #[test]
    fn responses_message_media_preserves_typed_metadata_and_order() {
        use querymt::chat::{ChatMessagePart, MediaKind, MediaPart, MediaSource, MediaType};

        let provider = responses_provider();

        let mut inline = MediaPart::new(
            MediaKind::Image,
            Some("image/png; charset=binary".parse::<MediaType>().unwrap()),
            MediaSource::Inline {
                data: vec![1, 2, 3],
            },
        )
        .unwrap();
        inline.detail = Some("high".to_string());

        let mut file = MediaPart::new(
            MediaKind::Document,
            Some("application/pdf".parse::<MediaType>().unwrap()),
            MediaSource::Inline {
                data: vec![0x25, 0x50],
            },
        )
        .unwrap();
        file.filename = Some("report.pdf".to_string());

        let message = tagged_assistant_turn(vec![
            ChatMessagePart::Text {
                text: "see attached".to_string(),
                annotations: Vec::new(),
                extensions: querymt::chat::Extensions::new(),
            },
            ChatMessagePart::Media(Box::new(inline)),
            ChatMessagePart::Media(Box::new(file)),
        ]);

        let req = provider
            .chat_request(&[message], None)
            .expect("typed media must serialize");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        let content = body["input"][0]["content"].as_array().unwrap();

        let kinds: Vec<&str> = content
            .iter()
            .map(|p| p["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, vec!["output_text", "input_image", "input_file"]);

        // Parsed MIME semantics survive (spelling may normalize), and MIME
        // parameters must not corrupt the `;base64` marker.
        let image_url = content[1]["image_url"].as_str().unwrap();
        assert!(
            image_url.starts_with("data:image/png;"),
            "unexpected data URL: {image_url}"
        );
        assert!(
            image_url.contains(";base64,"),
            "base64 marker intact: {image_url}"
        );
        let (metadata, _) = image_url
            .strip_prefix("data:")
            .unwrap()
            .split_once(',')
            .unwrap();
        assert!(
            metadata.contains("charset=binary"),
            "MIME parameter semantics preserved: {metadata}"
        );
        assert!(
            metadata
                .trim_end_matches(";base64")
                .contains("charset=binary"),
            "parameter precedes base64 marker: {metadata}"
        );

        assert_eq!(content[1]["detail"], Value::String("high".to_string()));
        assert_eq!(
            content[2]["filename"],
            Value::String("report.pdf".to_string())
        );
        assert!(
            content[2]["file_data"]
                .as_str()
                .unwrap()
                .starts_with("data:application/pdf;base64,")
        );
    }

    #[test]
    fn responses_message_media_preserves_url_source_form() {
        use querymt::chat::{ChatMessagePart, MediaKind, MediaPart, MediaSource};

        let provider = responses_provider();
        let url_media = MediaPart::new(
            MediaKind::Image,
            None,
            MediaSource::Url {
                url: "https://example.invalid/pic.png".to_string(),
            },
        )
        .unwrap();

        let message = tagged_assistant_turn(vec![ChatMessagePart::Media(Box::new(url_media))]);
        let req = provider.chat_request(&[message], None).unwrap();
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        assert_eq!(
            body["input"][0]["content"][0]["image_url"],
            Value::String("https://example.invalid/pic.png".to_string())
        );
    }

    #[test]
    fn responses_message_media_rejects_cross_origin_provider_reference() {
        use querymt::chat::{
            ChatMessagePart, ChatOutputProvenance, MediaKind, MediaPart, MediaSource, MediaType,
        };

        let provider = responses_provider();
        let reference = MediaPart::new(
            MediaKind::Document,
            Some("application/pdf".parse::<MediaType>().unwrap()),
            MediaSource::ProviderFile {
                file_id: "file_abc".to_string(),
                origin: ChatOutputProvenance {
                    provider: "other".to_string(),
                    protocol: "responses".to_string(),
                    model: "model-x".to_string(),
                    endpoint: "https://other.invalid/v1/responses".to_string(),
                },
            },
        )
        .unwrap();

        let message = tagged_assistant_turn(vec![ChatMessagePart::Media(Box::new(reference))]);
        let error = provider
            .chat_request(&[message], None)
            .expect_err("cross-origin provider reference must not be forwarded as a URL");
        assert!(
            matches!(error, LLMError::InvalidRequest(ref msg) if msg.contains("file_abc")),
            "expected explicit provider-reference error, got {error:?}"
        );
    }

    #[test]
    fn responses_retains_builtin_media_output_as_opaque_and_never_executes_it() {
        // Regression guard for task 5.10: an `image_generation_call` carries a
        // recognizable MIME-ish `result`, but no typed codec exists yet, so the
        // full item must stay opaque: no fabricated message media, no local
        // execution, and the payload preserved for a future explicit codec.
        use querymt::chat::{ChatOutputItem, ChatOutputStatus, StructuredStreamEvent};

        let provider = responses_provider();

        // Non-streaming path.
        let body = serde_json::json!({
            "id": "resp_builtin",
            "status": "completed",
            "output": [
                {
                    "type": "image_generation_call",
                    "id": "ig_1",
                    "status": "completed",
                    "result": "iVBORw0KGgoAAAANSUhEUg==",
                    "revised_prompt": "a cat"
                }
            ]
        });
        let response = provider
            .parse_chat(responses_json_response(body))
            .expect("built-in media response should parse");
        let output = &response;

        let ChatOutputItem::Opaque(opaque) = &output.items[0] else {
            panic!("expected opaque item");
        };
        assert_eq!(opaque.original_type, "image_generation_call");
        assert_eq!(
            opaque.payload.get("result").and_then(Value::as_str),
            Some("iVBORw0KGgoAAAANSUhEUg=="),
            "encoded bytes retained verbatim for a future codec"
        );
        assert!(
            response.text().is_none(),
            "opaque item must not be fabricated into message content"
        );
        assert!(
            response.tool_calls().is_none(),
            "opaque item must not enter the local function executor"
        );
        // A completed response with no local calls is a stop, not tool execution.
        assert_eq!(
            response.finish_reason,
            Some(querymt::chat::FinishReason::Stop)
        );
        // Opaque payloads are redacted from ordinary debug output.
        assert!(!format!("{opaque:?}").contains("iVBORw0KGgoAAAANSUhEUg=="));

        // Streaming path retains the same opaque item and emits no call events.
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut streamed = Vec::new();
        streamed.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.created","response":{"id":"resp_builtin"}}"#,
                ))
                .unwrap(),
        );
        streamed.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"image_generation_call","id":"ig_1","result":"iVBORw0KGgoAAAANSUhEUg=="}}"#,
                ))
                .unwrap(),
        );
        streamed.extend(
            parser
                .parse_chunk(&sse(
                    r#"{"type":"response.completed","response":{"id":"resp_builtin","status":"completed"}}"#,
                ))
                .unwrap(),
        );

        let streamed_opaque = streamed.iter().find_map(|chunk| match chunk {
            StreamChunk::Structured(StructuredStreamEvent::ItemCompleted { item, .. }) => {
                Some(item)
            }
            _ => None,
        });
        let Some(ChatOutputItem::Opaque(streamed_item)) = streamed_opaque else {
            panic!("expected opaque streamed item");
        };
        assert_eq!(streamed_item.original_type, "image_generation_call");
        assert_eq!(
            streamed_item.payload.get("result").and_then(Value::as_str),
            Some("iVBORw0KGgoAAAANSUhEUg==")
        );

        let terminal = streamed.iter().find_map(|chunk| match chunk {
            StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                status,
                finish_reason,
                ..
            }) => Some((*status, *finish_reason)),
            _ => None,
        });
        assert_eq!(
            terminal,
            Some((
                ChatOutputStatus::Completed,
                Some(querymt::chat::FinishReason::Stop)
            )),
            "opaque built-in item must not be reported as a pending tool call"
        );
    }

    #[test]
    fn responses_rejects_required_replay_of_unsupported_builtin_output() {
        // When such an item is required for continuation, construction must fail
        // explicitly rather than silently dropping the provider action.
        use querymt::chat::{ChatOpaqueItem, ChatOutputItem};

        let provider = responses_provider();
        let output = querymt::chat::ChatOutput {
            items: vec![ChatOutputItem::Opaque(ChatOpaqueItem {
                original_type: "image_generation_call".to_string(),
                payload: serde_json::json!({
                    "type": "image_generation_call",
                    "id": "ig_1",
                    "result": "iVBORw0KGgoAAAANSUhEUg=="
                }),
            })],
            ..querymt::chat::ChatOutput::default()
        };

        let mut assistant =
            ChatMessage::from_assistant_output(querymt::chat::ChatOutput::default());
        assert!(assistant.replace_output(output).is_ok());

        let error = provider
            .chat_request(&[assistant], None)
            .expect_err("unsupported built-in replay must fail explicitly");
        assert!(
            matches!(
                error,
                LLMError::InvalidRequest(ref msg) if msg.contains("image_generation_call")
            ),
            "expected explicit unsupported-continuation error, got {error:?}"
        );
    }

    /// Build a validated inline attachment result part.
    fn attachment_part(kind: MediaKind, mime_type: String, data: Vec<u8>) -> ToolResultPart {
        let media = MediaPart::new(
            kind,
            Some(mime_type.parse().expect("valid media type")),
            MediaSource::Inline { data },
        )
        .expect("valid inline attachment");
        ToolResultPart::Attachment(Box::new(media))
    }

    fn responses_provider() -> OpenAI {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini",
            "api_mode": "responses"
        });
        serde_json::from_value(cfg).unwrap()
    }

    /// Mixed reasoning/message/call output replays in order and is not flattened.
    #[test]
    fn responses_replays_structured_output_in_order() {
        use querymt::chat::{
            ChatFunctionCallItem, ChatMessageItem, ChatOutputItem, ChatReasoningItem,
            ChatReasoningPart, Extensions,
        };

        let provider = responses_provider();

        let output = querymt::chat::ChatOutput {
            provenance: Some(querymt::chat::ChatOutputProvenance {
                provider: "openai".into(),
                protocol: "responses".into(),
                model: "gpt-4o-mini".into(),
                endpoint: "https://api.openai.com/v1/responses".into(),
            }),
            items: vec![
                ChatOutputItem::Reasoning(ChatReasoningItem {
                    id: Some("rs_1".to_string()),
                    summary: vec![ChatReasoningPart::text("thinking about it")],
                    content: Vec::new(),
                    encrypted_content: Some("enc_payload".to_string()),
                    signature: None,
                    status: None,
                    extensions: Extensions::new(),
                }),
                ChatOutputItem::Message(ChatMessageItem {
                    id: Some("msg_1".to_string()),
                    role: ChatRole::Assistant,
                    phase: None,
                    status: None,
                    parts: vec![querymt::chat::ChatMessagePart::Text {
                        text: "calling the tool".to_string(),
                        annotations: Vec::new(),
                        extensions: Extensions::new(),
                    }],
                    extensions: Extensions::new(),
                }),
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("fc_item_1".to_string()),
                    call_id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"a.txt"}"#.to_string(),
                    status: None,
                    extensions: Extensions::new(),
                }),
                ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                    item_id: Some("fc_item_2".to_string()),
                    call_id: "call_2".to_string(),
                    name: "read_file".to_string(),
                    arguments: r#"{"path":"b.txt"}"#.to_string(),
                    status: None,
                    extensions: Extensions::new(),
                }),
            ],
            ..querymt::chat::ChatOutput::default()
        };

        let assistant = ChatMessage::from_assistant_output(output);

        let messages = vec![
            ChatMessage::from_user_parts(vec![ChatInputPart::text("read both".to_string())]),
            assistant,
            ChatMessage::from_user_parts(vec![ChatInputPart::tool_result(ToolResult {
                call_id: "call_1".to_string(),
                name: None,
                is_error: false,
                parts: vec![ToolResultPart::text("body a")],
                extensions: Default::default(),
            })]),
        ];

        let req = provider
            .chat_request(&messages, None)
            .expect("responses replay should build");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        let input = body["input"].as_array().unwrap();

        let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "message",
                "reasoning",
                "message",
                "function_call",
                "function_call",
                "function_call_output"
            ],
            "structured replay must retain item order, got {types:?}"
        );

        // Reasoning summaries stay visible and separate from encrypted state.
        assert_eq!(input[1]["id"], Value::String("rs_1".to_string()));
        assert_eq!(
            input[1]["encrypted_content"],
            Value::String("enc_payload".to_string())
        );
        assert_eq!(
            input[1]["summary"][0]["text"],
            Value::String("thinking about it".to_string())
        );

        // Distinct item and call IDs are preserved; arguments are byte-exact.
        assert_eq!(input[3]["call_id"], Value::String("call_1".to_string()));
        assert_eq!(input[4]["call_id"], Value::String("call_2".to_string()));
        assert_eq!(
            input[3]["arguments"],
            Value::String(r#"{"path":"a.txt"}"#.to_string())
        );
        assert_eq!(
            input[4]["arguments"],
            Value::String(r#"{"path":"b.txt"}"#.to_string())
        );

        // Each result references its original call id, with no duplicate items.
        assert_eq!(input[5]["call_id"], Value::String("call_1".to_string()));
        let call_count = types.iter().filter(|t| **t == "function_call").count();
        assert_eq!(call_count, 2, "no duplicate projected function calls");
    }

    #[test]
    fn responses_replays_portable_calls_without_native_metadata() {
        let provider = responses_provider();
        let output = querymt::chat::ChatOutput::from_projections(
            None,
            Some("calling".into()),
            Some(vec![querymt::ToolCall {
                id: "call_portable".into(),
                call_type: "function".into(),
                function: querymt::FunctionCall {
                    name: "lookup".into(),
                    arguments: r#"{"q":"rust"}"#.into(),
                },
            }]),
            None,
            Some(querymt::chat::FinishReason::ToolCalls),
        );
        let messages = vec![
            ChatMessage::from_assistant_output(output),
            ChatMessage::user()
                .tool_result_value(ToolResult::text("call_portable", "result"))
                .build(),
        ];

        let request = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_portable");
        assert_eq!(input[1]["name"], "lookup");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_portable");
    }

    #[test]
    fn responses_cross_origin_strips_encrypted_reasoning() {
        use querymt::chat::{
            ChatOutputItem, ChatOutputProvenance, ChatReasoningItem, ChatReasoningPart,
        };

        let provider = responses_provider();
        let output = querymt::chat::ChatOutput {
            provenance: Some(ChatOutputProvenance {
                provider: "openai".into(),
                protocol: "responses".into(),
                model: "other-model".into(),
                endpoint: "https://other.example/v1/responses".into(),
            }),
            items: vec![ChatOutputItem::Reasoning(ChatReasoningItem {
                id: Some("reasoning_1".into()),
                summary: vec![ChatReasoningPart::text("visible")],
                content: Vec::new(),
                encrypted_content: Some("must-not-leak".into()),
                signature: None,
                status: None,
                extensions: Default::default(),
            })],
            ..querymt::chat::ChatOutput::default()
        };

        let request = provider
            .chat_request(&[ChatMessage::from_assistant_output(output)], None)
            .unwrap();
        let body = String::from_utf8(request.into_body()).unwrap();
        assert!(body.contains("visible"));
        assert!(!body.contains("must-not-leak"));
        assert!(!body.contains("reasoning_1"));
    }

    /// Invalid function arguments are replayed byte-exact, not reparsed.
    #[test]
    fn responses_replays_raw_invalid_arguments_byte_exact() {
        use querymt::chat::{ChatFunctionCallItem, ChatOutputItem, Extensions};

        let provider = responses_provider();
        let raw = r#"{"path": [1,}"#;
        let output = querymt::chat::ChatOutput {
            items: vec![ChatOutputItem::FunctionCall(ChatFunctionCallItem {
                item_id: Some("fc_1".to_string()),
                call_id: "call_9".to_string(),
                name: "read_file".to_string(),
                arguments: raw.to_string(),
                status: None,
                extensions: Extensions::new(),
            })],
            ..querymt::chat::ChatOutput::default()
        };

        let mut assistant =
            ChatMessage::from_assistant_output(querymt::chat::ChatOutput::default());
        assert!(assistant.replace_output(output).is_ok());

        let req = provider
            .chat_request(&[assistant], None)
            .expect("replay should build");
        let body: Value = serde_json::from_slice(req.body()).unwrap();
        assert_eq!(
            body["input"][0]["arguments"],
            Value::String(raw.to_string()),
            "invalid JSON arguments must not become an executable empty object"
        );
    }

    /// Unknown required continuation fails instead of being silently dropped.
    #[test]
    fn responses_rejects_unsupported_unknown_continuation() {
        use querymt::chat::{ChatOpaqueItem, ChatOutputItem};

        let provider = responses_provider();
        let output = querymt::chat::ChatOutput {
            items: vec![ChatOutputItem::Opaque(ChatOpaqueItem {
                original_type: "future_action".to_string(),
                payload: serde_json::json!({"required": true}),
            })],
            ..querymt::chat::ChatOutput::default()
        };

        let mut assistant =
            ChatMessage::from_assistant_output(querymt::chat::ChatOutput::default());
        assert!(assistant.replace_output(output).is_ok());

        let error = provider
            .chat_request(&[assistant], None)
            .expect_err("unsupported unknown continuation must fail");
        assert!(
            matches!(error, LLMError::InvalidRequest(ref msg) if msg.contains("future_action")),
            "expected explicit unsupported-continuation error, got {error:?}"
        );
    }

    /// A message whose projected content disagrees with its authoritative output
    /// must not silently resend the stale payload.
    ///
    /// The exclusive payload makes that state unrepresentable through the public
    /// API, so the remaining entry point is a transitional serialized record: the
    /// migration boundary rejects it instead of choosing a representation.
    #[test]
    fn responses_request_rejects_stale_portable_projection() {
        use querymt::chat::{ChatOutputItem, Extensions};

        let output = querymt::chat::ChatOutput {
            items: vec![ChatOutputItem::Message(querymt::chat::ChatMessageItem {
                id: None,
                role: querymt::chat::ChatRole::Assistant,
                phase: None,
                status: None,
                parts: vec![querymt::chat::ChatMessagePart::Text {
                    text: "secret".to_string(),
                    annotations: Vec::new(),
                    extensions: Extensions::new(),
                }],
                extensions: Extensions::new(),
            })],
            ..querymt::chat::ChatOutput::default()
        };

        // A transitional `{ role, content, output }` record whose content is not
        // the output's projection is rejected at the load boundary.
        let stale = serde_json::json!({
            "role": "Assistant",
            "content": [{"type": "text", "text": "redacted"}],
            "output": serde_json::to_value(&output).unwrap()
        });
        let error = serde_json::from_value::<ChatMessage>(stale)
            .expect_err("stale duplicate representation must be rejected");
        assert!(
            error.to_string().contains("does not match"),
            "expected an explicit consistency error, got {error}"
        );
    }

    /// Reconcile `response.completed`: a stream that never sends
    /// `output_item.done` still yields completed items from the final snapshot.
    #[test]
    fn responses_stream_completed_reconciles_missing_item_done() {
        use querymt::chat::{ChatOutputStatus, ChatStreamAccumulator};

        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut accumulator = ChatStreamAccumulator::new();

        for payload in [
            r#"{"type":"response.created","response":{"id":"resp_rec","model":"gpt-4o-mini"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_rec","content":[]}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"reconciled"}"#,
            // Note: no output_item.done. The final snapshot must be reconciled.
            r#"{"type":"response.completed","response":{"id":"resp_rec","status":"completed","output":[{"type":"message","id":"msg_rec","status":"completed","content":[{"type":"output_text","text":"reconciled"}]}]}}"#,
        ] {
            for chunk in parser.parse_chunk(&sse(payload)).unwrap() {
                accumulator.push(&chunk).expect("accumulator accepts event");
            }
        }

        let output = accumulator
            .finish_success()
            .expect("terminal snapshot reconciles missing output_item.done");
        assert_eq!(output.status, Some(ChatOutputStatus::Completed));
        assert_eq!(output.text().as_deref(), Some("reconciled"));
    }

    /// A function call that appears only in the final completed snapshot must be
    /// reconciled before terminal classification so it yields `ToolCalls` and is
    /// dispatchable exactly once.
    #[test]
    fn responses_stream_final_snapshot_only_function_call_yields_tool_calls() {
        use querymt::chat::{ChatOutputStatus, ChatStreamAccumulator, FinishReason};

        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut accumulator = ChatStreamAccumulator::new();

        for payload in [
            r#"{"type":"response.created","response":{"id":"resp_fc","model":"gpt-4o-mini"}}"#,
            // The call is absent from the deltas; it appears only in the final snapshot.
            r#"{"type":"response.completed","response":{"id":"resp_fc","status":"completed","output":[{"type":"function_call","id":"item_fc","call_id":"call_fc","name":"lookup","arguments":"{\"q\":\"rust\"}","status":"completed"}]}}"#,
        ] {
            for chunk in parser.parse_chunk(&sse(payload)).unwrap() {
                accumulator.push(&chunk).expect("accumulator accepts event");
            }
        }

        let output = accumulator
            .finish_success()
            .expect("final-snapshot call reconciles before terminal classification");
        assert_eq!(output.status, Some(ChatOutputStatus::Completed));
        assert_eq!(
            output.finish_reason,
            Some(FinishReason::ToolCalls),
            "a call discovered only in the final snapshot must indicate pending tool execution"
        );
        let calls = output.tool_calls().expect("call available for execution");
        assert_eq!(calls.len(), 1, "call is dispatched exactly once");
        assert_eq!(calls[0].id, "call_fc");
        assert_eq!(calls[0].function.name, "lookup");
    }

    /// An incomplete streaming response must retain its partial canonical output
    /// together with the terminal cause, and unfinished calls must never be
    /// executable.
    #[test]
    fn responses_stream_incomplete_retains_partial_output_and_cause() {
        use querymt::chat::{ChatOutputStatus, ChatStreamAccumulator, ChatStreamFinish};

        let provider = responses_provider();
        let mut parser = provider.chat_stream_parser().unwrap();
        let mut accumulator = ChatStreamAccumulator::new();

        for payload in [
            r#"{"type":"response.created","response":{"id":"resp_inc","model":"gpt-4o-mini"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_inc","content":[]}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":"partial answer"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_inc","status":"incomplete","content":[{"type":"output_text","text":"partial answer"}]}}"#,
            r#"{"type":"response.incomplete","response":{"id":"resp_inc","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}"#,
        ] {
            for chunk in parser.parse_chunk(&sse(payload)).unwrap() {
                accumulator.push(&chunk).expect("accumulator accepts event");
            }
        }

        match accumulator.finish() {
            ChatStreamFinish::Incomplete { output, detail } => {
                assert_eq!(
                    detail.as_deref(),
                    Some("max_output_tokens"),
                    "terminal cause is retained"
                );
                assert_eq!(output.status, Some(ChatOutputStatus::Incomplete));
                assert_eq!(
                    output.text().as_deref(),
                    Some("partial answer"),
                    "partial output remains inspectable"
                );
            }
            other => panic!("expected incomplete outcome, got {other:?}"),
        }
    }

    #[test]
    fn base_url_is_normalized_to_trailing_slash() {
        let cfg = serde_json::json!({
            "api_key": "",
            "base_url": "http://localhost:8000/v1",
            "model": "gpt-4o-mini"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        assert_eq!(provider.base_url.as_str(), "http://localhost:8000/v1/");
        let joined = provider.base_url.join("audio/transcriptions").unwrap();
        assert_eq!(
            joined.as_str(),
            "http://localhost:8000/v1/audio/transcriptions"
        );
    }

    #[test]
    fn chat_stream_request_forces_stream_true() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "base_url": "https://token-plan-sgp.xiaomimimo.com/v1",
            "model": "mimo-v2.5-pro"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();

        let req = provider
            .chat_stream_request(&[], None)
            .expect("stream request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body should be valid json");
        assert_eq!(body.get("stream"), Some(&Value::Bool(true)));
    }

    #[test]
    fn stream_parser_returns_classified_error() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        let mut parser = provider.chat_stream_parser().unwrap();
        let chunk = br#"data: {"error":{"message":"busy","code":"server_error"}}

"#;

        let error = parser.parse_chunk(chunk).unwrap_err();
        assert!(matches!(error, LLMError::ProviderResponseError(_)));
    }

    #[test]
    fn stream_parsers_are_isolated_per_stream() {
        let cfg = serde_json::json!({
            "api_key": "test-key",
            "model": "gpt-4o-mini"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();

        let mut parser_a = provider
            .chat_stream_parser()
            .expect("parser A should initialize");
        let mut parser_b = provider
            .chat_stream_parser()
            .expect("parser B should initialize");

        let a_delta = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}
"#;
        let b_delta = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_b","type":"function","function":{"name":"write_file","arguments":"{\"path\":"}}]}}]}
"#;

        let a_more = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}
"#;
        let b_more = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"b.txt\"}"}}]}}]}
"#;

        let a_done = br#"data: [DONE]
"#;
        let b_done = br#"data: [DONE]
"#;

        let _ = parser_a.parse_chunk(a_delta).expect("parse A delta");
        let _ = parser_b.parse_chunk(b_delta).expect("parse B delta");
        let _ = parser_a.parse_chunk(a_more).expect("parse A more");
        let _ = parser_b.parse_chunk(b_more).expect("parse B more");

        let a_events = parser_a.parse_chunk(a_done).expect("parse A done");
        let b_events = parser_b.parse_chunk(b_done).expect("parse B done");

        let a_complete = a_events.iter().find_map(|chunk| {
            if let StreamChunk::ToolUseComplete { tool_call, .. } = chunk {
                Some(tool_call)
            } else {
                None
            }
        });
        let b_complete = b_events.iter().find_map(|chunk| {
            if let StreamChunk::ToolUseComplete { tool_call, .. } = chunk {
                Some(tool_call)
            } else {
                None
            }
        });

        let a_complete = a_complete.expect("A should emit ToolUseComplete");
        let b_complete = b_complete.expect("B should emit ToolUseComplete");

        assert_eq!(a_complete.id, "call_a");
        assert_eq!(a_complete.function.name, "read_file");
        assert_eq!(a_complete.function.arguments, r#"{"path":"a.txt"}"#);

        assert_eq!(b_complete.id, "call_b");
        assert_eq!(b_complete.function.name, "write_file");
        assert_eq!(b_complete.function.arguments, r#"{"path":"b.txt"}"#);
    }
}

/// Creates an OpenAI HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(OpenAIFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(OpenAIFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{OpenAI, OpenAIFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = OpenAI,
        factory = OpenAIFactory,
        name   = "openai",
    }
}
