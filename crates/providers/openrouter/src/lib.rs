use http::{Method, Request, Response, header::CONTENT_TYPE};
use qmt_openai::api::{
    OpenAIErrorClassification, OpenAIProviderConfig, OpenAIToolUseState,
    classify_openai_http_error_with, openai_chat_request, openai_embed_request,
    openai_parse_chat_with, openai_parse_embed, parse_openai_sse_chunk_with, url_schema,
};
use querymt::{
    HTTPLLMProvider,
    chat::{
        ChatMessage, ChatResponse, StreamChunk, StructuredOutputFormat, Tool, ToolChoice,
        http::{ChatStreamParser, HTTPChatProvider},
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::{LLMError, ProviderErrorKind},
    plugin::HTTPLLMProviderFactory,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OpenRouter {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "OpenRouter::default_base_url")]
    pub base_url: Url,
    pub api_key: String,
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
}

impl OpenAIProviderConfig for OpenRouter {
    fn api_key(&self) -> &str {
        &self.api_key
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
}

fn normalize_openrouter_error_type(error_type: &str) -> String {
    error_type
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
}

fn openrouter_error_kind(error_type: &str) -> Option<ProviderErrorKind> {
    match error_type {
        "provider_overloaded" => Some(ProviderErrorKind::ServerOverloaded),
        "rate_limit_exceeded" => Some(ProviderErrorKind::RateLimited),
        "context_length_exceeded" => Some(ProviderErrorKind::ContextWindowExceeded),
        "authentication" => Some(ProviderErrorKind::Authentication),
        "payment_required" => Some(ProviderErrorKind::QuotaExceeded),
        "invalid_request"
        | "invalid_prompt"
        | "not_found"
        | "precondition_failed"
        | "payload_too_large"
        | "unprocessable"
        | "content_policy_violation"
        | "refusal"
        | "invalid_image"
        | "image_too_large"
        | "image_too_small"
        | "unsupported_image_format"
        | "image_not_found" => Some(ProviderErrorKind::InvalidRequest),
        "permission_denied" | "token_limit_exceeded" | "string_too_long" => {
            Some(ProviderErrorKind::UnknownPermanent)
        }
        "provider_unavailable" | "image_download_failed" | "server" | "timeout" | "unmapped" => {
            Some(ProviderErrorKind::UnknownTransient)
        }
        // These describe successful length-limited completions when OpenRouter
        // applies its documented transformation, not retryable provider errors.
        "max_tokens_exceeded" => Some(ProviderErrorKind::UnknownPermanent),
        _ => None,
    }
}

fn classify_openrouter_error(error: &Value) -> Option<OpenAIErrorClassification> {
    let typed_error = error
        .get("metadata")
        .and_then(|metadata| metadata.get("error_type"))
        .or_else(|| error.get("error_type"))
        .and_then(Value::as_str)
        .map(normalize_openrouter_error_type);

    if let Some(error_type) = typed_error.as_deref()
        && let Some(kind) = openrouter_error_kind(error_type)
    {
        return Some(OpenAIErrorClassification {
            kind,
            error_type: typed_error,
        });
    }

    let status = error.get("code").and_then(|code| {
        code.as_u64()
            .and_then(|code| u16::try_from(code).ok())
            .or_else(|| code.as_str().and_then(|code| code.parse::<u16>().ok()))
    })?;
    let kind = match status {
        400 | 403 | 404 | 422 => ProviderErrorKind::InvalidRequest,
        401 => ProviderErrorKind::Authentication,
        402 => ProviderErrorKind::QuotaExceeded,
        408 | 500..=599 => ProviderErrorKind::UnknownTransient,
        429 => ProviderErrorKind::RateLimited,
        _ => return None,
    };
    Some(OpenAIErrorClassification {
        kind,
        error_type: typed_error,
    })
}

fn normalize_openrouter_chat_error_response(response: Response<Vec<u8>>) -> Response<Vec<u8>> {
    if !response.status().is_success() {
        return response;
    }

    let Ok(mut envelope) = serde_json::from_slice::<Value>(response.body()) else {
        return response;
    };
    if envelope.get("error").is_some() {
        return response;
    }
    let choice_error = envelope
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| {
            choice.get("error").or_else(|| {
                choice
                    .get("message")
                    .and_then(|message| message.get("error"))
            })
        })
        .cloned();
    let Some(choice_error) = choice_error else {
        return response;
    };
    let Some(object) = envelope.as_object_mut() else {
        return response;
    };
    object.insert("error".to_owned(), choice_error);

    let (parts, _) = response.into_parts();
    Response::from_parts(
        parts,
        serde_json::to_vec(&envelope).expect("JSON value must serialize"),
    )
}

impl HTTPChatProvider for OpenRouter {
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_openai_http_error_with(response, Some(classify_openrouter_error))
    }

    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        openai_chat_request(self, messages, tools)
    }

    fn chat_stream_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut cfg = self.clone();
        cfg.stream = Some(true);
        openai_chat_request(&cfg, messages, tools)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        openai_parse_chat_with(
            self,
            normalize_openrouter_chat_error_response(response),
            Some(classify_openrouter_error),
        )
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(OpenRouterStreamParser::default()))
    }
}

#[derive(Default)]
struct OpenRouterStreamParser {
    tool_states: HashMap<usize, OpenAIToolUseState>,
}

impl ChatStreamParser for OpenRouterStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        parse_openai_sse_chunk_with(
            chunk,
            &mut self.tool_states,
            Some(classify_openrouter_error),
        )
    }
}

impl HTTPEmbeddingProvider for OpenRouter {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for OpenRouter {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for OpenRouter {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl OpenRouter {
    fn default_base_url() -> Url {
        Url::parse("https://openrouter.ai/api/v1/").unwrap()
    }
}

struct OpenRouterFactory;

impl HTTPLLMProviderFactory for OpenRouterFactory {
    fn name(&self) -> &str {
        "openrouter"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("OPENROUTER_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => OpenRouter::default_base_url(),
        };
        let models_url = base_url.join("models")?;
        Ok(Request::builder()
            .method(Method::GET)
            .uri(models_url.to_string())
            .header(CONTENT_TYPE, "application/json")
            .body(Vec::new())?)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        let resp_json: Value = serde_json::from_slice(resp.body())?;
        let arr = resp_json
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| LLMError::InvalidRequest("`models` missing or not an array".into()))?;

        let names = arr
            .iter()
            .filter_map(|m| m.get("id"))
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();

        Ok(names)
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(OpenRouter);
        // Extract the schema object and turn it into a JSON string
        serde_json::to_string(&schema).expect("OpenRouter JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: OpenRouter = serde_json::from_str(cfg)?;

        // 2) Done—our OpenAI::send/chat/etc methods will lazily build the Client
        Ok(Box::new(provider))
    }
}

/// Creates an OpenRouter HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(OpenRouterFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(OpenRouterFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{OpenRouter, OpenRouterFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = OpenRouter,
        factory = OpenRouterFactory,
        name   = "openrouter",
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenRouter, OpenRouterFactory};
    use http::Response;
    use querymt::chat::{StreamChunk, http::HTTPChatProvider};
    use querymt::{
        error::{LLMError, ProviderErrorKind},
        plugin::HTTPLLMProviderFactory,
    };
    use serde_json::Value;

    fn test_provider() -> OpenRouter {
        serde_json::from_value(serde_json::json!({
            "api_key": "test-key",
            "model": "openai/gpt-4o-mini"
        }))
        .unwrap()
    }

    #[test]
    fn malformed_config_is_non_retryable_json_error() {
        let error = OpenRouterFactory
            .from_config("{")
            .err()
            .expect("config should be rejected");

        assert!(!error.is_retryable());
        assert!(matches!(error, LLMError::JsonError(_)));
    }

    #[test]
    fn supports_streaming() {
        assert!(test_provider().supports_streaming());
    }

    #[test]
    fn chat_stream_request_forces_stream_true() {
        let provider = test_provider();

        let req = provider
            .chat_stream_request(&[], None)
            .expect("stream request should build");
        let body: Value = serde_json::from_slice(req.body()).expect("body should be valid json");
        assert_eq!(body.get("stream"), Some(&Value::Bool(true)));
    }

    fn parse_stream_error(error: Value) -> LLMError {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("parser should initialize");
        let chunk = format!("data: {}\n\n", serde_json::json!({ "error": error }));
        parser
            .parse_chunk(chunk.as_bytes())
            .expect_err("error envelope should return an error")
    }

    #[test]
    fn upstream_idle_timeout_is_retryable() {
        for code in [serde_json::json!(504), serde_json::json!("504")] {
            let error = parse_stream_error(serde_json::json!({
                "message": "Upstream idle timeout exceeded",
                "code": code
            }));
            assert!(matches!(
                error,
                LLMError::ProviderResponseError(ref failure)
                    if failure.kind() == ProviderErrorKind::UnknownTransient
                        && failure.code() == Some("504")
            ));
            assert!(error.is_retryable());
        }
    }

    #[test]
    fn typed_error_takes_precedence_over_numeric_status() {
        let error = parse_stream_error(serde_json::json!({
            "message": "Request was rejected",
            "code": 504,
            "metadata": { "error_type": "invalid_request" }
        }));

        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::InvalidRequest
                    && failure.error_type() == Some("invalid_request")
                    && failure.code() == Some("504")
        ));
        assert!(!error.is_retryable());
    }

    #[test]
    fn typed_generic_errors_are_retryable() {
        for error_type in ["timeout", "server", "unmapped"] {
            let error = parse_stream_error(serde_json::json!({
                "message": "provider failed",
                "code": 500,
                "metadata": { "error_type": error_type }
            }));
            assert!(matches!(
                error,
                LLMError::ProviderResponseError(ref failure)
                    if failure.kind() == ProviderErrorKind::UnknownTransient
                        && failure.error_type() == Some(error_type)
            ));
            assert!(error.is_retryable(), "error_type={error_type}");
        }
    }

    #[test]
    fn typed_rate_limit_preserves_retry_hint() {
        let error = parse_stream_error(serde_json::json!({
            "message": "slow down",
            "code": 429,
            "retry_after": "3s",
            "metadata": { "error_type": "rate_limit_exceeded" }
        }));

        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::RateLimited
                    && failure.retry_after_secs() == Some(3)
        ));
    }

    #[test]
    fn successful_http_error_body_uses_openrouter_classifier() {
        let provider = test_provider();
        let response = Response::builder()
            .status(200)
            .body(
                br#"{"id":"gen-1","error":{"code":504,"message":"timed out","metadata":{"error_type":"timeout"}}}"#
                    .to_vec(),
            )
            .unwrap();

        let error = provider
            .parse_chat(response)
            .err()
            .expect("HTTP 200 error body should fail");
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::UnknownTransient
                    && failure.error_type() == Some("timeout")
                    && failure.request_id() == Some("gen-1")
        ));
    }

    #[test]
    fn choice_level_http_error_uses_openrouter_classifier() {
        let provider = test_provider();
        let response = Response::builder()
            .status(200)
            .body(
                br#"{"id":"gen-2","choices":[{"message":{"role":"assistant","content":"partial"},"finish_reason":"error","error":{"code":504,"message":"timed out","metadata":{"error_type":"timeout"}}}]}"#
                    .to_vec(),
            )
            .unwrap();

        let error = provider
            .parse_chat(response)
            .err()
            .expect("choice-level error should fail");
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::UnknownTransient
                    && failure.error_type() == Some("timeout")
        ));
    }

    #[test]
    fn stream_parsers_are_isolated_per_stream() {
        let provider = test_provider();

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
