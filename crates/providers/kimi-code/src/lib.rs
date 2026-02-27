use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use kimi_auth::kimi_cli_oauth_config;
use qmt_openai::api::{
    OpenAIProviderConfig, OpenAIToolUseState, openai_chat_request, openai_parse_chat,
    parse_openai_sse_chunk, url_schema,
};
use querymt::{
    HTTPLLMProvider,
    auth::ApiKeyResolver,
    chat::{
        ChatMessage, ChatResponse, ChatRole, MessageType, StreamChunk, StructuredOutputFormat,
        Tool, ToolChoice, http::HTTPChatProvider,
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
use std::sync::{Arc, Mutex};
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
    /// Stateful buffer for assembling streamed tool calls across SSE chunks.
    #[serde(skip)]
    #[schemars(skip)]
    pub tool_state_buffer: Arc<Mutex<HashMap<usize, OpenAIToolUseState>>>,
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

    fn reasoning_effort(&self) -> Option<&String> {
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
        let mut request = openai_chat_request(&resolved, messages, tools)?;
        KimiCode::inject_tool_call_reasoning_content(&mut request, messages)?;
        KimiCode::apply_kimi_agent_headers(&mut request, &profile)?;
        Ok(request)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        openai_parse_chat(self, response)
    }

    fn parse_chat_stream_chunk(&self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        log::trace!(
            "kimi-code SSE chunk ({} bytes): {:?}",
            chunk.len(),
            String::from_utf8_lossy(chunk)
        );
        let normalized = KimiCode::normalize_sse_data_prefix(chunk);
        let mut tool_states = self.tool_state_buffer.lock().unwrap();
        parse_openai_sse_chunk(&normalized, &mut tool_states)
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

    fn inject_tool_call_reasoning_content(
        request: &mut Request<Vec<u8>>,
        source_messages: &[ChatMessage],
    ) -> Result<(), LLMError> {
        let mut body: Value = serde_json::from_slice(request.body()).map_err(|e| {
            LLMError::InvalidRequest(format!("failed to decode kimi request JSON body: {e}"))
        })?;

        let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
            return Ok(());
        };

        let mut reasoning_by_tool_id: HashMap<&str, &str> = HashMap::new();
        for msg in source_messages {
            if let (ChatRole::Assistant, MessageType::ToolUse(calls)) =
                (&msg.role, &msg.message_type)
            {
                let thinking = msg.thinking.as_deref().unwrap_or_default();
                for call in calls {
                    reasoning_by_tool_id.insert(&call.id, thinking);
                }
            }
        }

        for msg in messages {
            let Some(obj) = msg.as_object_mut() else {
                continue;
            };
            if obj.get("role").and_then(Value::as_str) != Some("assistant")
                || !obj.get("tool_calls").is_some_and(|v| !v.is_null())
            {
                continue;
            }

            if !KimiCode::is_reasoning_content_missing(obj.get("reasoning_content")) {
                continue;
            }

            let from_source = obj
                .get("tool_calls")
                .and_then(Value::as_array)
                .and_then(|calls| {
                    calls.iter().find_map(|tc| {
                        let id = tc.get("id").and_then(Value::as_str)?;
                        let r = *reasoning_by_tool_id.get(id)?;
                        if r.trim().is_empty() { None } else { Some(r) }
                    })
                });

            let content_fallback = obj
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_default();

            let value = from_source.unwrap_or(content_fallback);
            let reasoning_content = if value.trim().is_empty() {
                "Tool call reasoning unavailable."
            } else {
                value
            };
            obj.insert(
                "reasoning_content".into(),
                Value::String(reasoning_content.into()),
            );
        }

        *request.body_mut() = serde_json::to_vec(&body).map_err(|e| {
            LLMError::InvalidRequest(format!("failed to encode kimi request JSON body: {e}"))
        })?;
        Ok(())
    }

    fn is_reasoning_content_missing(value: Option<&Value>) -> bool {
        match value {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => s.trim().is_empty(),
            Some(_) => false,
        }
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
        Some("KIMI_API_KEY".into())
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
            "kimi-latest".to_string(),
        ])
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(KimiCode);
        serde_json::to_string(&schema.schema).expect("KimiCode JSON Schema should always serialize")
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
    use querymt::{FunctionCall, ToolCall};
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

        let messages = vec![ChatMessage::user().content("hello").build()];
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
    fn chat_request_injects_reasoning_content_for_assistant_tool_calls() {
        let provider = test_provider();
        let messages = vec![
            ChatMessage::user().content("run tool").build(),
            ChatMessage::assistant()
                .tool_use(vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "run".to_string(),
                        arguments: "{}".to_string(),
                    },
                }])
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
    fn chat_request_injects_non_empty_reasoning_content_when_missing() {
        let provider = test_provider();
        let messages = vec![
            ChatMessage::user().content("run tool").build(),
            ChatMessage::assistant()
                .tool_use(vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "run".to_string(),
                        arguments: "{}".to_string(),
                    },
                }])
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
            Some("Tool call reasoning unavailable.")
        );
    }

    #[test]
    fn reasoning_content_matches_by_tool_call_id_not_position() {
        let provider = test_provider();
        let messages = vec![
            ChatMessage::user().content("first").build(),
            ChatMessage::assistant()
                .tool_use(vec![ToolCall {
                    id: "call_a".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "alpha".into(),
                        arguments: "{}".into(),
                    },
                }])
                .thinking("reasoning for alpha")
                .build(),
            ChatMessage::user().content("second").build(),
            ChatMessage::assistant()
                .tool_use(vec![ToolCall {
                    id: "call_b".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "beta".into(),
                        arguments: "{}".into(),
                    },
                }])
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
        let messages = vec![ChatMessage::user().content("hi").build()];
        let request = provider.chat_request(&messages, None).unwrap();
        let body: Value = serde_json::from_slice(request.body()).unwrap();
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn parse_chat_stream_chunk_emits_text_delta() {
        let provider = test_provider();
        let chunk =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello world\"}}]}\n\n";
        let events = provider.parse_chat_stream_chunk(chunk).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            querymt::chat::StreamChunk::Text(text) => assert_eq!(text, "hello world"),
            other => panic!("expected Text chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_emits_reasoning_content_as_thinking() {
        let provider = test_provider();
        // Kimi uses `reasoning_content` for thinking deltas in SSE responses
        let chunk = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"let me think...\"}}]}\n\n";
        let events = provider.parse_chat_stream_chunk(chunk).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            querymt::chat::StreamChunk::Thinking(text) => assert_eq!(text, "let me think..."),
            other => panic!("expected Thinking chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_handles_tool_call_sequence() {
        let provider = test_provider();

        // First chunk: tool call start with id and function name
        let chunk1 = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n";
        let events1 = provider.parse_chat_stream_chunk(chunk1).unwrap();
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
        let events2 = provider.parse_chat_stream_chunk(chunk2).unwrap();
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
        let events3 = provider.parse_chat_stream_chunk(chunk3).unwrap();
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
            matches!(e, querymt::chat::StreamChunk::Done { stop_reason } if stop_reason == "tool_use")
        });
        assert!(
            has_done,
            "expected Done with stop_reason 'tool_use' in {events3:?}"
        );
    }

    #[test]
    fn parse_chat_stream_chunk_handles_done_sentinel() {
        let provider = test_provider();
        let chunk = b"data: [DONE]\n\n";
        let events = provider.parse_chat_stream_chunk(chunk).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            querymt::chat::StreamChunk::Done { stop_reason } => {
                assert_eq!(stop_reason, "end_turn");
            }
            other => panic!("expected Done chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_chat_stream_chunk_emits_usage() {
        let provider = test_provider();
        let chunk =
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20}}\n\n";
        let events = provider.parse_chat_stream_chunk(chunk).unwrap();
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
        let chunk = b"data:{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\n";
        let events = provider.parse_chat_stream_chunk(chunk).unwrap();
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
        let chunk = b"data:[DONE]\n";
        let events = provider.parse_chat_stream_chunk(chunk).unwrap();
        assert_eq!(
            events.len(),
            1,
            "expected Done from data:[DONE], got {events:?}"
        );
        match &events[0] {
            querymt::chat::StreamChunk::Done { stop_reason } => {
                assert_eq!(stop_reason, "end_turn");
            }
            other => panic!("expected Done chunk, got {other:?}"),
        }
    }

    #[test]
    fn is_reasoning_content_missing_edge_cases() {
        assert!(KimiCode::is_reasoning_content_missing(None));
        assert!(KimiCode::is_reasoning_content_missing(Some(&Value::Null)));
        assert!(KimiCode::is_reasoning_content_missing(Some(
            &Value::String("".into())
        )));
        assert!(KimiCode::is_reasoning_content_missing(Some(
            &Value::String("   ".into())
        )));
        assert!(!KimiCode::is_reasoning_content_missing(Some(
            &Value::String("thinking".into())
        )));
        assert!(!KimiCode::is_reasoning_content_missing(Some(&Value::Bool(
            false
        ))));
        assert!(!KimiCode::is_reasoning_content_missing(Some(
            &serde_json::json!(42)
        )));
    }
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
