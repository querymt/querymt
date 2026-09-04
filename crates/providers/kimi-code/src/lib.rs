use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use kimi_auth::kimi_cli_oauth_config;
use qmt_openai::api::{
    OpenAIProviderConfig, OpenAIToolUseState, classify_openai_http_error, openai_chat_request,
    openai_parse_chat, parse_openai_sse_chunk, url_schema,
};
use querymt::{
    HTTPLLMProvider,
    auth::ApiKeyResolver,
    chat::{
        ChatMessage, ChatResponse, Content, StreamChunk, StructuredOutputFormat, Tool, ToolChoice,
        http::{ChatStreamParser, HTTPChatProvider},
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct KimiCode {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "KimiCode::default_base_url")]
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
    pub n: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    /// JSON schema for structured output
    pub json_schema: Option<StructuredOutputFormat>,
    /// Optional resolver for dynamic credential refresh (e.g., OAuth tokens).
    #[serde(skip)]
    #[schemars(skip)]
    pub key_resolver: Option<Arc<dyn ApiKeyResolver>>,
    #[serde(skip)]
    #[schemars(skip)]
    pub kimi_profile: Option<kimi_auth::OAuthConfig>,
}

impl OpenAIProviderConfig for KimiCode {
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
        None
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
        let mut map = Map::new();
        if let Some(presence_penalty) = self.presence_penalty {
            map.insert("presence_penalty".into(), presence_penalty.into());
        }
        if let Some(frequency_penalty) = self.frequency_penalty {
            map.insert("frequency_penalty".into(), frequency_penalty.into());
        }
        if let Some(n) = self.n {
            map.insert("n".into(), n.into());
        }
        if !map.is_empty() {
            return Some(map);
        }

        None
    }
}

impl HTTPChatProvider for KimiCode {
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        classify_openai_http_error(response)
    }

    fn supports_streaming(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut resolved = self.clone();
        resolved.api_key = self.resolved_api_key();
        let profile = self.profile();
        let normalized_messages = KimiCode::normalize_messages(messages);
        let mut request = openai_chat_request(&resolved, &normalized_messages, tools)?;
        KimiCode::apply_kimi_agent_headers(&mut request, &profile)?;
        Ok(request)
    }

    fn chat_stream_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut resolved = self.clone();
        resolved.api_key = self.resolved_api_key();
        resolved.stream = Some(true);
        let profile = self.profile();
        let normalized_messages = KimiCode::normalize_messages(messages);
        let mut request = openai_chat_request(&resolved, &normalized_messages, tools)?;
        KimiCode::apply_kimi_agent_headers(&mut request, &profile)?;
        Ok(request)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        openai_parse_chat(self, response)
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(KimiCodeStreamParser::default()))
    }
}

#[derive(Default)]
struct KimiCodeStreamParser {
    tool_states: HashMap<usize, OpenAIToolUseState>,
}

impl ChatStreamParser for KimiCodeStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        log::trace!(
            "kimi-code SSE chunk ({} bytes): {:?}",
            chunk.len(),
            String::from_utf8_lossy(chunk)
        );
        let normalized = KimiCode::normalize_sse_data_prefix(chunk);
        let mut chunks = parse_openai_sse_chunk(&normalized, &mut self.tool_states)?;

        // Kimi may omit tool call IDs and internally address those calls as
        // `<function-name>:<index>`. Persist that ID so the subsequent tool
        // response uses the same identifier when the conversation is replayed.
        for chunk in &mut chunks {
            match chunk {
                StreamChunk::ToolUseStart { index, id, name } if id.is_empty() => {
                    *id = format!("{name}:{index}");
                    if let Some(state) = self.tool_states.get_mut(index) {
                        state.id.clone_from(id);
                    }
                }
                StreamChunk::ToolUseComplete { index, tool_call } if tool_call.id.is_empty() => {
                    tool_call.id = format!("{}:{index}", tool_call.function.name);
                }
                _ => {}
            }
        }

        Ok(chunks)
    }
}

impl HTTPEmbeddingProvider for KimiCode {
    fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        unimplemented!("feature is missing!")
    }

    fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        unimplemented!("feature is missing!")
    }
}

impl HTTPCompletionProvider for KimiCode {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for KimiCode {
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

impl KimiCode {
    fn default_base_url() -> Url {
        Url::parse("https://api.kimi.com/coding/v1/").unwrap()
    }

    fn normalize_messages(messages: &[ChatMessage]) -> Vec<ChatMessage> {
        let mut normalized = Vec::with_capacity(messages.len());

        for message in messages {
            if message.content.iter().any(Content::is_tool_result) {
                let mut tool_results = message.clone();
                let supplemental = tool_results
                    .content
                    .iter()
                    .filter(|block| !block.is_tool_result())
                    .cloned()
                    .collect::<Vec<_>>();
                tool_results.content.retain(Content::is_tool_result);
                let role = tool_results.role.clone();
                let cache = tool_results.cache.clone();
                normalized.push(tool_results);

                if !supplemental.is_empty() {
                    normalized.push(ChatMessage {
                        role,
                        content: supplemental,
                        cache,
                    });
                }
            } else {
                normalized.push(message.clone());
            }
        }

        normalized
    }

    fn resolved_api_key(&self) -> String {
        if let Some(ref resolver) = self.key_resolver {
            resolver.current()
        } else {
            self.api_key.clone()
        }
    }

    fn profile(&self) -> kimi_auth::OAuthConfig {
        self.kimi_profile
            .clone()
            .unwrap_or_else(kimi_cli_oauth_config)
    }

    fn apply_kimi_agent_headers(
        request: &mut Request<Vec<u8>>,
        profile: &kimi_auth::OAuthConfig,
    ) -> Result<(), LLMError> {
        let mut set_header = |name: &'static str, value: &str| -> Result<(), LLMError> {
            let value = http::header::HeaderValue::from_str(value).map_err(|e| {
                LLMError::InvalidRequest(format!("invalid header value for '{name}': {e}"))
            })?;
            request.headers_mut().insert(name, value);
            Ok(())
        };

        let msh_version = &profile.app_version;
        let user_agent =
            std::env::var("KIMI_USER_AGENT").unwrap_or_else(|_| format!("KimiCLI/{msh_version}"));

        set_header("user-agent", &user_agent)?;
        set_header("x-msh-platform", &profile.platform)?;
        set_header("x-msh-version", msh_version)?;
        set_header("x-msh-device-name", &profile.device_name)?;
        set_header("x-msh-device-model", &profile.device_model)?;
        set_header("x-msh-os-version", &profile.os_version)?;
        set_header("x-msh-device-id", &profile.device_id)?;
        Ok(())
    }

    /// Normalizes SSE lines so that `data:{...}` (no space after colon) becomes
    /// `data: {...}`.  The shared OpenAI SSE parser expects the `data: ` prefix
    /// with a trailing space; some servers (including Kimi) may omit it.
    fn normalize_sse_data_prefix(chunk: &[u8]) -> Vec<u8> {
        let text = String::from_utf8_lossy(chunk);
        let mut out = String::with_capacity(text.len());
        for line in text.split('\n') {
            let trimmed = line.trim_start();
            if trimmed.starts_with("data:") && !trimmed.starts_with("data: ") {
                out.push_str("data: ");
                out.push_str(&trimmed["data:".len()..]);
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        out.into_bytes()
    }
}

struct KimiCodeFactory;

impl HTTPLLMProviderFactory for KimiCodeFactory {
    fn name(&self) -> &str {
        "kimi-code"
    }

    fn api_key_name(&self) -> Option<String> {
        None
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => KimiCode::default_base_url(),
        };
        let models_url = base_url.join("models")?;
        let api_key = cfg
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(models_url.to_string())
            .header(CONTENT_TYPE, "application/json");

        if !api_key.is_empty() {
            builder = builder.header(AUTHORIZATION, format!("Bearer {api_key}"));
        }

        let mut request = builder.body(Vec::new())?;
        let profile = kimi_cli_oauth_config();
        KimiCode::apply_kimi_agent_headers(&mut request, &profile)?;
        Ok(request)
    }

    fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Ok(vec![
            "kimi-k2-0711-preview".to_string(),
            "kimi-k2-0905-preview".to_string(),
            "kimi-k2-thinking".to_string(),
            "kimi-k2-thinking-turbo".to_string(),
            "kimi-k2-turbo-preview".to_string(),
            "kimi-k2.5".to_string(),
            "kimi-k2.6".to_string(),
            "kimi-k2.7".to_string(),
            "kimi-k3".to_string(),
        ])
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(KimiCode);
        serde_json::to_string(&schema).expect("KimiCode JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let mut provider: KimiCode = serde_json::from_str(cfg)?;
        provider.kimi_profile = Some(kimi_cli_oauth_config());
        Ok(Box::new(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::KimiCode;
    use querymt::chat::{ChatMessage, http::HTTPChatProvider};
    use serde_json::Value;

    fn test_provider() -> KimiCode {
        serde_json::from_value(serde_json::json!({
            "api_key": "test-token",
            "model": "kimi-latest"
        }))
        .unwrap()
    }

    #[test]
    fn chat_request_includes_kimi_agent_headers() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");

        let messages = vec![ChatMessage::user().text("hello").build()];
        let request = provider.chat_request(&messages, None).unwrap();

        for header_name in [
            "user-agent",
            "x-msh-platform",
            "x-msh-version",
            "x-msh-device-name",
            "x-msh-device-model",
            "x-msh-os-version",
            "x-msh-device-id",
        ] {
            let header_value = request
                .headers()
                .get(header_name)
                .unwrap_or_else(|| panic!("missing header: {header_name}"));
            assert!(!header_value.as_bytes().is_empty());
        }
    }

    #[test]
    fn stream_request_emits_context_after_tool_response_batch() {
        use querymt::chat::Content;

        let provider = test_provider();
        let messages = vec![
            ChatMessage::assistant()
                .tool_use("call_1", "ls", serde_json::json!({"path": "."}))
                .build(),
            ChatMessage::user()
                .tool_result(
                    "call_1".to_string(),
                    Some("ls".to_string()),
                    false,
                    vec![Content::text("specs.md")],
                )
                .text("<run-objective>Collect benchmark data</run-objective>")
                .build(),
        ];

        let request = provider.chat_stream_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let api_messages = body["messages"].as_array().unwrap();

        assert_eq!(api_messages.len(), 3);
        assert_eq!(api_messages[1]["role"], "tool");
        assert_eq!(api_messages[1]["tool_call_id"], "call_1");
        assert_eq!(api_messages[1]["content"], "specs.md");
        assert_eq!(api_messages[2]["role"], "user");
        assert!(
            api_messages[2]["content"]
                .as_str()
                .unwrap()
                .contains("<run-objective>")
        );
    }

    #[test]
    fn non_stream_request_emits_context_after_tool_response_batch() {
        use querymt::chat::Content;

        let provider = test_provider();
        let messages = vec![
            ChatMessage::assistant()
                .tool_use("call_1", "ls", serde_json::json!({"path": "."}))
                .build(),
            ChatMessage::user()
                .tool_result(
                    "call_1".to_string(),
                    Some("ls".to_string()),
                    false,
                    vec![Content::text("specs.md")],
                )
                .text("<run-objective>Collect benchmark data</run-objective>")
                .build(),
        ];

        let request = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let api_messages = body["messages"].as_array().unwrap();

        assert_eq!(api_messages.len(), 3);
        assert_eq!(api_messages[1]["role"], "tool");
        assert_eq!(api_messages[1]["tool_call_id"], "call_1");
        assert_eq!(api_messages[1]["content"], "specs.md");
        assert_eq!(api_messages[2]["role"], "user");
        assert!(
            api_messages[2]["content"]
                .as_str()
                .unwrap()
                .contains("<run-objective>")
        );
    }

    #[test]
    fn chat_request_injects_reasoning_content_for_assistant_tool_calls() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let messages = vec![
            ChatMessage::user().text("run tool").build(),
            ChatMessage::assistant()
                .tool_use("call_1", "run", serde_json::json!({}))
                .thinking("need to run tool")
                .build(),
        ];

        let request = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
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

        assert_eq!(
            assistant_tool_msg
                .get("reasoning_content")
                .and_then(Value::as_str),
            Some("need to run tool")
        );
    }

    #[test]
    fn chat_request_omits_reasoning_content_when_no_thinking() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let messages = vec![
            ChatMessage::user().text("run tool").build(),
            ChatMessage::assistant()
                .tool_use("call_1", "run", serde_json::json!({}))
                .build(),
        ];

        let request = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
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

        // With the model-layer implementation, reasoning_content is omitted
        // when there is no thinking content — no longer injected via JSON hack.
        assert!(
            assistant_tool_msg
                .get("reasoning_content")
                .and_then(Value::as_str)
                .is_none()
        );
    }

    #[test]
    fn reasoning_content_matches_by_tool_call_id_not_position() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let messages = vec![
            ChatMessage::user().text("first").build(),
            ChatMessage::assistant()
                .tool_use("call_a", "alpha", serde_json::json!({}))
                .thinking("reasoning for alpha")
                .build(),
            ChatMessage::user().text("second").build(),
            ChatMessage::assistant()
                .tool_use("call_b", "beta", serde_json::json!({}))
                .thinking("reasoning for beta")
                .build(),
        ];

        let request = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let api_messages = body["messages"].as_array().unwrap();

        let tool_msgs: Vec<&Value> = api_messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(Value::as_str) == Some("assistant")
                    && m.get("tool_calls").is_some()
            })
            .collect();

        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(
            tool_msgs[0]["reasoning_content"].as_str(),
            Some("reasoning for alpha")
        );
        assert_eq!(
            tool_msgs[1]["reasoning_content"].as_str(),
            Some("reasoning for beta")
        );
    }

    #[test]
    fn supports_streaming_defaults_to_false() {
        // Default (stream: None) → no streaming
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        assert!(!provider.supports_streaming());

        // Explicit stream: true → streaming enabled
        let provider: KimiCode = serde_json::from_value(serde_json::json!({
            "api_key": "test-token",
            "model": "kimi-latest",
            "stream": true
        }))
        .unwrap();
        assert!(provider.supports_streaming());

        // Explicit stream: false → streaming disabled
        let provider: KimiCode = serde_json::from_value(serde_json::json!({
            "api_key": "test-token",
            "model": "kimi-latest",
            "stream": false
        }))
        .unwrap();
        assert!(!provider.supports_streaming());
    }

    #[test]
    fn stream_config_defaults_to_false_in_request() {
        // When stream is omitted, the request body should have stream: false
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let messages = vec![ChatMessage::user().text("hi").build()];
        let request = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn chat_stream_request_forces_stream_true() {
        let provider = test_provider();
        let messages = vec![ChatMessage::user().text("hi").build()];
        let request = provider.chat_stream_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn parse_chat_stream_chunk_emits_text_delta() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let chunk =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello world\"}}]}\n\n";
        let events = parser.parse_chunk(chunk).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            querymt::chat::StreamChunk::Text(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Text chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_emits_reasoning_content_as_thinking() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        // Kimi uses `reasoning_content` for thinking deltas in SSE responses
        let chunk = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"let me think...\"}}]}\n\n";
        let events = parser.parse_chunk(chunk).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            querymt::chat::StreamChunk::Thinking(text) => assert_eq!(text, "let me think..."),
            other => panic!("expected Thinking chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_handles_tool_call_sequence() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");

        // First chunk: tool call start with id and function name
        let chunk1 = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n";
        let events1 = parser.parse_chunk(chunk1).unwrap();
        assert_eq!(events1.len(), 1);
        match &events1[0] {
            querymt::chat::StreamChunk::ToolUseStart { index, id, name } => {
                assert_eq!(*index, 0);
                assert_eq!(id, "call_abc");
                assert_eq!(name, "get_weather");
            }
            other => panic!("expected ToolUseStart, got {other:?}"),
        }

        // Second chunk: arguments delta
        let chunk2 = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}]}}]}\n\n";
        let events2 = parser.parse_chunk(chunk2).unwrap();
        assert_eq!(events2.len(), 1);
        match &events2[0] {
            querymt::chat::StreamChunk::ToolUseInputDelta {
                index,
                partial_json,
            } => {
                assert_eq!(*index, 0);
                assert_eq!(partial_json, "{\"city\":\"Paris\"}");
            }
            other => panic!("expected ToolUseInputDelta, got {other:?}"),
        }

        // Final chunk: finish_reason triggers ToolUseComplete + Done
        let chunk3 = b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let events3 = parser.parse_chunk(chunk3).unwrap();
        assert!(
            events3.len() >= 2,
            "expected at least 2 events, got {events3:?}"
        );

        let has_tool_complete = events3.iter().any(|e| {
            matches!(
                e,
                querymt::chat::StreamChunk::ToolUseComplete { index: 0, .. }
            )
        });
        assert!(has_tool_complete, "expected ToolUseComplete in {events3:?}");

        let has_done = events3.iter().any(|e| {
            matches!(e, querymt::chat::StreamChunk::Done { finish_reason } if *finish_reason == querymt::chat::FinishReason::ToolCalls)
        });
        assert!(
            has_done,
            "expected Done with FinishReason::ToolCalls in {events3:?}"
        );
    }

    #[test]
    fn non_streaming_response_preserves_opaque_tool_call_id() {
        let provider = test_provider();
        let response = http::Response::builder()
            .status(200)
            .body(
                br#"{"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":"tool_LUbH2dHPwNU9pt1GrIYrywKV","type":"function","function":{"name":"ls","arguments":"{\"path\":\".\"}"}}]}}]}"#
                    .to_vec(),
            )
            .unwrap();

        let response = provider.parse_chat(response).unwrap();
        let tool_calls = response.tool_calls().expect("tool calls should be present");

        assert_eq!(tool_calls[0].id, "tool_LUbH2dHPwNU9pt1GrIYrywKV");
    }

    #[test]
    fn streaming_tool_call_id_is_stable_across_response_and_replay() {
        use querymt::chat::{Content, StreamChunk};

        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");

        let start = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"tool_LUbH2dHPwNU9pt1GrIYrywKV","type":"function","function":{"name":"ls","arguments":"{\"path\":\".\"}"}}]}}]}

"#;
        let start_events = parser.parse_chunk(start).unwrap();
        assert!(matches!(
            &start_events[0],
            StreamChunk::ToolUseStart { index: 0, id, name }
                if id == "tool_LUbH2dHPwNU9pt1GrIYrywKV" && name == "ls"
        ));

        let finish = br#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

"#;
        let complete = parser
            .parse_chunk(finish)
            .unwrap()
            .into_iter()
            .find_map(|chunk| match chunk {
                StreamChunk::ToolUseComplete { tool_call, .. } => Some(tool_call),
                _ => None,
            })
            .expect("tool call should complete");
        assert_eq!(complete.id, "tool_LUbH2dHPwNU9pt1GrIYrywKV");

        let messages = vec![
            ChatMessage::assistant()
                .tool_use(
                    complete.id.clone(),
                    complete.function.name,
                    serde_json::from_str(&complete.function.arguments).unwrap(),
                )
                .build(),
            ChatMessage::user()
                .tool_result(
                    complete.id,
                    Some("ls".to_string()),
                    false,
                    vec![Content::text("specs.md")],
                )
                .build(),
        ];
        let request = provider.chat_stream_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let api_messages = body["messages"].as_array().unwrap();

        assert_eq!(api_messages.len(), 2);
        assert_eq!(
            api_messages[0]["tool_calls"][0]["id"],
            "tool_LUbH2dHPwNU9pt1GrIYrywKV"
        );
        assert_eq!(
            api_messages[1]["tool_call_id"],
            "tool_LUbH2dHPwNU9pt1GrIYrywKV"
        );
    }

    #[test]
    fn replay_preserves_existing_opaque_tool_call_ids() {
        use querymt::chat::Content;

        let provider = test_provider();
        let opaque_id = "tool_LUbH2dHPwNU9pt1GrIYrywKV";
        let messages = vec![
            ChatMessage::assistant()
                .tool_use(opaque_id, "ls", serde_json::json!({"path": "."}))
                .build(),
            ChatMessage::user()
                .tool_result(
                    opaque_id.to_string(),
                    Some("ls".to_string()),
                    false,
                    vec![Content::text("specs.md")],
                )
                .build(),
        ];

        let request = provider.chat_stream_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let api_messages = body["messages"].as_array().unwrap();

        assert_eq!(api_messages[0]["tool_calls"][0]["id"], opaque_id);
        assert_eq!(api_messages[1]["tool_call_id"], opaque_id);
    }

    #[test]
    fn replay_preserves_multiple_tool_call_ids_and_result_order() {
        use querymt::chat::Content;

        let provider = test_provider();
        let messages = vec![
            ChatMessage::assistant()
                .tool_use("opaque-ls-1", "ls", serde_json::json!({"path": "."}))
                .tool_use("opaque-read", "read_tool", serde_json::json!({"path": "a"}))
                .tool_use("opaque-ls-2", "ls", serde_json::json!({"path": "src"}))
                .build(),
            ChatMessage::user()
                .tool_result(
                    "opaque-read".to_string(),
                    Some("read_tool".to_string()),
                    false,
                    vec![Content::text("a")],
                )
                .tool_result(
                    "opaque-ls-2".to_string(),
                    Some("ls".to_string()),
                    false,
                    vec![Content::text("src/lib.rs")],
                )
                .tool_result(
                    "opaque-ls-1".to_string(),
                    Some("ls".to_string()),
                    false,
                    vec![Content::text("a")],
                )
                .build(),
        ];

        let request = provider.chat_stream_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        let api_messages = body["messages"].as_array().unwrap();

        let tool_calls = api_messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls[0]["id"], "opaque-ls-1");
        assert_eq!(tool_calls[1]["id"], "opaque-read");
        assert_eq!(tool_calls[2]["id"], "opaque-ls-2");
        assert_eq!(api_messages[1]["tool_call_id"], "opaque-read");
        assert_eq!(api_messages[2]["tool_call_id"], "opaque-ls-2");
        assert_eq!(api_messages[3]["tool_call_id"], "opaque-ls-1");
    }

    #[test]
    fn parse_chat_stream_chunk_handles_done_sentinel() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let chunk = b"data: [DONE]\n\n";
        let events = parser.parse_chunk(chunk).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            querymt::chat::StreamChunk::Done { finish_reason } => {
                assert_eq!(*finish_reason, querymt::chat::FinishReason::Stop);
            }
            other => panic!("expected Done chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_emits_usage() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let chunk =
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20}}\n\n";
        let events = parser.parse_chunk(chunk).unwrap();
        let usage_event = events
            .iter()
            .find(|e| matches!(e, querymt::chat::StreamChunk::Usage(_)));
        assert!(usage_event.is_some(), "expected Usage chunk in {events:?}");
        match usage_event.unwrap() {
            querymt::chat::StreamChunk::Usage(usage) => {
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.output_tokens, 20);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_handles_data_prefix_without_space() {
        // Kimi may send `data:{...}` instead of `data: {...}`
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let chunk = b"data:{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n";
        let events = parser.parse_chunk(chunk).unwrap();
        assert_eq!(
            events.len(),
            1,
            "expected 1 event from data: without space, got {events:?}"
        );
        match &events[0] {
            querymt::chat::StreamChunk::Text(text) => assert_eq!(text, "hello"),
            other => panic!("expected Text chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_handles_done_without_space() {
        let provider = test_provider();
        let mut parser = provider
            .chat_stream_parser()
            .expect("stream parser should initialize");
        let chunk = b"data:[DONE]\n";
        let events = parser.parse_chunk(chunk).unwrap();
        assert_eq!(
            events.len(),
            1,
            "expected Done from data:[DONE], got {events:?}"
        );
        match &events[0] {
            querymt::chat::StreamChunk::Done { finish_reason } => {
                assert_eq!(*finish_reason, querymt::chat::FinishReason::Stop);
            }
            other => panic!("expected Done chunk, got {other:?}"),
        }
    }
}

/// Creates a Kimi Code HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(KimiCodeFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(KimiCodeFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{KimiCode, KimiCodeFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = KimiCode,
        factory = KimiCodeFactory,
        name   = "kimi-code",
    }
}
