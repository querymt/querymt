use http::{Request, Response};
use qmt_openai::api::{
    OpenAIErrorClassification, OpenAIProviderConfig, OpenAIToolUseState,
    classify_openai_http_error_with, openai_chat_request, openai_embed_request,
    openai_list_models_request, openai_parse_chat_with, openai_parse_embed_with,
    openai_parse_list_models, parse_openai_sse_chunk_with, url_schema,
};
use querymt::error::ProviderErrorKind;
use querymt::{
    HTTPLLMProvider,
    chat::{
        ChatCall, ChatCapabilities, ChatOutput, StreamChunk, StructuredOutputFormat, Tool,
        ToolChoice,
        http::{ChatStreamParser, HTTPChatProvider, PreparedChatRequest},
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
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

fn classify_zai_error(error: &Value) -> Option<OpenAIErrorClassification> {
    let code = error.get("code")?;
    let code = code
        .as_str()
        .map(str::to_owned)
        .or_else(|| code.as_i64().map(|code| code.to_string()))?;

    (code == "1311").then_some(OpenAIErrorClassification {
        kind: ProviderErrorKind::QuotaExceeded,
        error_type: None,
    })
}

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Zai {
    #[schemars(schema_with = "url_schema")]
    #[serde(
        default = "Zai::default_base_url",
        deserialize_with = "deserialize_base_url"
    )]
    pub base_url: Url,
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default, deserialize_with = "querymt::params::deserialize_system_vec")]
    #[schemars(schema_with = "querymt::params::schema_for_system_prompts")]
    pub system: Vec<String>,
    pub timeout_seconds: Option<u64>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub json_schema: Option<StructuredOutputFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, Value>>,
}

impl OpenAIProviderConfig for Zai {
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

    fn system(&self) -> std::borrow::Cow<'_, [String]> {
        std::borrow::Cow::Borrowed(&self.system)
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
        None
    }

    fn embedding_dimensions(&self) -> Option<&u32> {
        None
    }

    fn reasoning_effort(&self) -> Option<querymt::chat::ReasoningEffort> {
        None
    }

    fn json_schema(&self) -> Option<&StructuredOutputFormat> {
        self.json_schema.as_ref()
    }

    fn extra_body(&self) -> Option<serde_json::Map<String, Value>> {
        self.extra_body.clone()
    }
}

impl HTTPChatProvider for Zai {
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_openai_http_error_with(response, Some(classify_zai_error))
    }

    fn prepare_chat_request(
        &self,
        call: &ChatCall<'_>,
        stream: bool,
    ) -> Result<PreparedChatRequest, LLMError> {
        let request = if stream {
            let mut cfg = self.clone();
            cfg.stream = Some(true);
            openai_chat_request(&cfg, call.messages, call.tools)
        } else {
            openai_chat_request(self, call.messages, call.tools)
        }?;
        Ok(PreparedChatRequest::stateless(request))
    }

    fn parse_chat(
        &self,
        response: Response<Vec<u8>>,
        _context: serde_json::Value,
    ) -> Result<ChatOutput, LLMError> {
        openai_parse_chat_with(self, response, Some(classify_zai_error))
    }

    fn chat_stream_parser(
        &self,
        _context: serde_json::Value,
    ) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(ZaiStreamParser::default()))
    }

    fn capabilities(&self) -> ChatCapabilities {
        ChatCapabilities::NONE.with_streaming(true)
    }
}

#[derive(Default)]
struct ZaiStreamParser {
    tool_states: HashMap<usize, OpenAIToolUseState>,
}

impl ChatStreamParser for ZaiStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        parse_openai_sse_chunk_with(chunk, &mut self.tool_states, Some(classify_zai_error))
    }
}

impl HTTPEmbeddingProvider for Zai {
    fn classify_embed_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_openai_http_error_with(response, Some(classify_zai_error))
    }

    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        openai_parse_embed_with(self, resp, Some(classify_zai_error))
    }
}

impl HTTPCompletionProvider for Zai {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for Zai {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl Zai {
    fn default_base_url() -> Url {
        Url::parse("https://api.z.ai/api/paas/v4/").unwrap()
    }
}

struct ZaiFactory;

impl HTTPLLMProviderFactory for ZaiFactory {
    fn name(&self) -> &str {
        "zai"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("ZAI_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => normalize_base_url(Url::parse(base_url_str)?),
            None => Zai::default_base_url(),
        };
        openai_list_models_request(&base_url, &cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(Zai);
        serde_json::to_string(&schema).expect("ZAI JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let mut provider: Zai = serde_json::from_str(cfg)?;
        provider.base_url = normalize_base_url(provider.base_url);
        Ok(Box::new(provider))
    }
}

/// Creates a Z.AI HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(ZaiFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(ZaiFactory)) as *mut _
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::error::ProviderErrorKind;
    use serde_json::json;

    fn provider() -> Zai {
        serde_json::from_value(json!({
            "api_key": "test-key",
            "model": "glm-4.5"
        }))
        .unwrap()
    }

    fn error_response(status: u16, code: Value) -> Response<Vec<u8>> {
        Response::builder()
            .status(status)
            .body(
                json!({
                    "error": {
                        "message": "model is not available for this subscription plan",
                        "code": code
                    }
                })
                .to_string()
                .into_bytes(),
            )
            .unwrap()
    }

    fn assert_error_kind(error: &LLMError, kind: ProviderErrorKind, retryable: bool) {
        match error {
            LLMError::ProviderResponseError(failure) => assert_eq!(failure.kind(), kind),
            other => panic!("expected provider response error, got {other}"),
        }
        assert_eq!(error.is_retryable(), retryable);
    }

    #[test]
    fn subscription_plan_error_is_permanent_for_http_envelopes_and_sse() {
        let provider = provider();
        for code in [json!(1311), json!("1311")] {
            let http_error = provider.classify_chat_error(&error_response(429, code.clone()));
            assert_error_kind(&http_error, ProviderErrorKind::QuotaExceeded, false);

            let embedding_http_error =
                provider.classify_embed_error(&error_response(429, code.clone()));
            assert_error_kind(
                &embedding_http_error,
                ProviderErrorKind::QuotaExceeded,
                false,
            );

            let body_error = provider
                .parse_chat(error_response(200, code.clone()), serde_json::Value::Null)
                .unwrap_err();
            assert_error_kind(&body_error, ProviderErrorKind::QuotaExceeded, false);

            let embedding_body_error = provider
                .parse_embed(error_response(200, code.clone()))
                .unwrap_err();
            assert_error_kind(
                &embedding_body_error,
                ProviderErrorKind::QuotaExceeded,
                false,
            );

            let event = json!({
                "error": {
                    "message": "model is not available for this subscription plan",
                    "code": code
                }
            });
            let chunk = format!("data: {event}\n\n");
            let mut parser = ZaiStreamParser::default();
            let stream_error = parser.parse_chunk(chunk.as_bytes()).unwrap_err();
            assert_error_kind(&stream_error, ProviderErrorKind::QuotaExceeded, false);
        }
    }

    #[test]
    fn rate_limit_error_remains_retryable_across_chat_paths() {
        let provider = provider();
        let response = |status| {
            Response::builder()
                .status(status)
                .body(
                    json!({
                        "error": {
                            "message": "rate limit reached",
                            "type": "rate_limit_error",
                            "code": "rate_limit_exceeded"
                        }
                    })
                    .to_string()
                    .into_bytes(),
                )
                .unwrap()
        };

        let http_error = provider.classify_chat_error(&response(429));
        assert_error_kind(&http_error, ProviderErrorKind::RateLimited, true);

        let body_error = provider
            .parse_chat(response(200), serde_json::Value::Null)
            .unwrap_err();
        assert_error_kind(&body_error, ProviderErrorKind::RateLimited, true);

        let chunk = format!(
            "data: {}\n\n",
            String::from_utf8(response(200).body().clone()).unwrap()
        );
        let mut parser = ZaiStreamParser::default();
        let stream_error = parser.parse_chunk(chunk.as_bytes()).unwrap_err();
        assert_error_kind(&stream_error, ProviderErrorKind::RateLimited, true);
    }
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Zai, ZaiFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Zai,
        factory = ZaiFactory,
        name   = "zai",
    }
}
