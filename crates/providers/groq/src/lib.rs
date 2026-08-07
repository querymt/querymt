use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use qmt_openai::api::{
    OpenAIProviderConfig, OpenAIToolUseState, openai_chat_request, openai_embed_request,
    openai_list_models_request, openai_parse_chat, openai_parse_embed, openai_parse_list_models,
    parse_openai_sse_chunk, url_schema,
};
use querymt::{
    HTTPLLMProvider, ToolCall,
    chat::{
        ChatMessage, ChatResponse, StreamChunk, StructuredOutputFormat, Tool, ToolChoice,
        http::{ChatStreamParser, HTTPChatProvider},
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    handle_http_error,
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
pub struct Groq {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Groq::default_base_url")]
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

#[derive(Serialize)]
struct GroqCompletionRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    suffix: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<&'a u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<&'a f32>,
}

#[derive(Deserialize)]
struct GroqCompletionResponse {
    model: String,
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    index: u32,
    message: AssistantMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct AssistantMessage {
    role: String,
    tool_calls: Option<Vec<ToolCall>>,
    content: String, //TODO: Either<String, Vec<String>>,
}

impl OpenAIProviderConfig for Groq {
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

    // Groq rejects `reasoning_content` on input assistant messages even when
    // models emit reasoning in responses (e.g. qwen3 tool loops).
    fn include_reasoning_content(&self) -> bool {
        false
    }

    fn json_schema(&self) -> Option<&StructuredOutputFormat> {
        self.json_schema.as_ref()
    }
}

impl HTTPChatProvider for Groq {
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
        Ok(Box::new(GroqStreamParser::default()))
    }
}

#[derive(Default)]
struct GroqStreamParser {
    tool_states: HashMap<usize, OpenAIToolUseState>,
}

impl ChatStreamParser for GroqStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        parse_openai_sse_chunk(chunk, &mut self.tool_states)
    }
}

impl HTTPEmbeddingProvider for Groq {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for Groq {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        let api_key = match self.api_key().into() {
            Some(key) => key,
            None => return Err(LLMError::AuthError("Missing API key".to_string())),
        };

        let body = GroqCompletionRequest {
            model: self.model(),
            prompt: &req.prompt,
            suffix: req.suffix.as_deref(),
            max_tokens: req.max_tokens.as_ref(),
            temperature: req.temperature.as_ref(),
        };

        let json_body = serde_json::to_vec(&body)?;
        let url = self
            .base_url()
            .join("fim/completions")
            .map_err(|e| LLMError::HttpError(e.to_string()))?;

        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.to_string())
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .header(CONTENT_TYPE, "application/json")
            .body(json_body)?)
    }

    fn parse_complete(&self, resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        handle_http_error!(resp);

        let json_resp: Result<GroqCompletionResponse, serde_json::Error> =
            serde_json::from_slice(resp.body());
        match json_resp {
            Ok(completion_response) => Ok(CompletionResponse {
                text: completion_response.choices[0].message.content.clone(), // FIXME
            }),
            Err(e) => Err(LLMError::from(e)),
        }
    }
}

impl HTTPLLMProvider for Groq {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl Groq {
    fn default_base_url() -> Url {
        Url::parse("https://api.groq.com/openai/v1/").unwrap()
    }
}

struct GroqFactory;

impl HTTPLLMProviderFactory for GroqFactory {
    fn name(&self) -> &str {
        "groq"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("GROQ_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => Groq::default_base_url(),
        };
        openai_list_models_request(&base_url, &cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(Groq);
        serde_json::to_string(&schema).expect("Groq JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Groq = serde_json::from_str(cfg)?;

        Ok(Box::new(provider))
    }
}

/// Creates a Groq HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(GroqFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(GroqFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Groq, GroqFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Groq,
        factory = GroqFactory,
        name   = "groq",
    }
}

#[cfg(test)]
mod tests {
    use super::Groq;
    use querymt::chat::{ChatMessage, StreamChunk, http::HTTPChatProvider};
    use serde_json::Value;

    fn test_provider() -> Groq {
        serde_json::from_value(serde_json::json!({
            "api_key": "test-key",
            "model": "llama-3.3-70b-versatile"
        }))
        .unwrap()
    }

    #[test]
    fn supports_streaming() {
        assert!(test_provider().supports_streaming());
    }

    #[test]
    fn chat_request_omits_reasoning_content_for_assistant_tool_calls() {
        let provider = test_provider();
        let messages = vec![
            ChatMessage::user()
                .text("what's this project about?")
                .build(),
            ChatMessage::assistant()
                .thinking("I should read the README.")
                .tool_use(
                    "call_1",
                    "read_tool",
                    serde_json::json!({"path": "README.md"}),
                )
                .build(),
        ];

        let request = provider
            .chat_request(&messages, None)
            .expect("request should build");
        let body: Value =
            serde_json::from_slice(request.body()).expect("body should be valid json");
        let api_messages = body
            .get("messages")
            .and_then(Value::as_array)
            .expect("messages array should be present");

        let assistant_tool_msg = api_messages
            .iter()
            .find(|msg| {
                msg.get("role").and_then(Value::as_str) == Some("assistant")
                    && msg.get("tool_calls").is_some()
            })
            .expect("assistant tool call message should be present");

        assert!(
            assistant_tool_msg.get("reasoning_content").is_none(),
            "Groq must not send reasoning_content on assistant messages"
        );
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
