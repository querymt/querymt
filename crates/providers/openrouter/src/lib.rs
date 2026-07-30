use http::{Method, Request, Response, header::CONTENT_TYPE};
use qmt_openai::api::{
    OpenAIProviderConfig, OpenAIToolUseState, classify_openai_http_error, openai_chat_request,
    openai_embed_request, openai_parse_chat, openai_parse_embed, parse_openai_sse_chunk,
    url_schema,
};
use querymt::{
    HTTPLLMProvider,
    chat::{
        ChatMessage, ChatResponse, StreamChunk, StructuredOutputFormat, Tool, ToolChoice,
        http::{ChatStreamParser, HTTPChatProvider},
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
    fn provider_name(&self) -> &str {
        "openrouter"
    }

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

impl HTTPChatProvider for OpenRouter {
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_openai_http_error(self.provider_name(), response)
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
        openai_parse_chat(self, response)
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
        parse_openai_sse_chunk("openrouter", chunk, &mut self.tool_states)
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
        let provider: OpenRouter = serde_json::from_str(cfg)
            .map_err(|e| LLMError::PluginError(format!("OpenRouter config error: {}", e)))?;

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
    use super::OpenRouter;
    use querymt::chat::{StreamChunk, http::HTTPChatProvider};
    use serde_json::Value;

    fn test_provider() -> OpenRouter {
        serde_json::from_value(serde_json::json!({
            "api_key": "test-key",
            "model": "openai/gpt-4o-mini"
        }))
        .unwrap()
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
