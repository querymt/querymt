use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use kimi_auth::kimi_cli_oauth_config;
use qmt_openai::api::{OpenAIProviderConfig, openai_chat_request, openai_parse_chat, url_schema};
use querymt::{
    HTTPLLMProvider,
    auth::ApiKeyResolver,
    chat::{
        ChatMessage, ChatResponse, ChatRole, MessageType, StructuredOutputFormat, Tool, ToolChoice,
        http::HTTPChatProvider,
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
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
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut resolved = self.clone();
        resolved.api_key = self.resolved_api_key();
        let mut request = openai_chat_request(&resolved, messages, tools)?;
        KimiCode::inject_tool_call_reasoning_content(&mut request, messages)?;
        KimiCode::apply_kimi_agent_headers(&mut request)?;
        Ok(request)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        openai_parse_chat(self, response)
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

        let mut reasoning_values = source_messages
            .iter()
            .filter_map(|msg| match (&msg.role, &msg.message_type) {
                (ChatRole::Assistant, MessageType::ToolUse(_)) => {
                    Some(msg.thinking.clone().unwrap_or_default())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .into_iter();

        for msg in messages {
            let Some(obj) = msg.as_object_mut() else {
                continue;
            };
            let is_assistant = obj.get("role").and_then(Value::as_str) == Some("assistant");
            let has_tool_calls = obj.get("tool_calls").is_some_and(|v| !v.is_null());
            if !is_assistant || !has_tool_calls {
                continue;
            }

            if !KimiCode::is_reasoning_content_missing(obj.get("reasoning_content")) {
                continue;
            }

            let from_message = reasoning_values.next().unwrap_or_default();
            let content_fallback = obj
                .get("content")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_default()
                .to_string();

            let reasoning_content =
                KimiCode::normalize_reasoning_content(if from_message.trim().is_empty() {
                    content_fallback
                } else {
                    from_message
                });
            obj.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning_content),
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

    fn normalize_reasoning_content(value: String) -> String {
        if value.trim().is_empty() {
            "Tool call reasoning unavailable.".to_string()
        } else {
            value
        }
    }

    fn apply_kimi_agent_headers(request: &mut Request<Vec<u8>>) -> Result<(), LLMError> {
        let mut set_header = |name: &'static str, value: String| -> Result<(), LLMError> {
            let value = http::header::HeaderValue::from_str(&value).map_err(|e| {
                LLMError::InvalidRequest(format!("invalid header value for '{name}': {e}"))
            })?;
            request.headers_mut().insert(name, value);
            Ok(())
        };

        let profile = kimi_cli_oauth_config();
        let msh_version = profile.app_version.clone();
        let user_agent =
            std::env::var("KIMI_USER_AGENT").unwrap_or_else(|_| format!("KimiCLI/{msh_version}"));

        set_header("user-agent", user_agent)?;
        set_header("x-msh-platform", profile.platform)?;
        set_header("x-msh-version", msh_version)?;
        set_header("x-msh-device-name", profile.device_name)?;
        set_header("x-msh-device-model", profile.device_model)?;
        set_header("x-msh-os-version", profile.os_version)?;
        set_header("x-msh-device-id", profile.device_id)?;
        Ok(())
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
        KimiCode::apply_kimi_agent_headers(&mut request)?;
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
        let provider: KimiCode = serde_json::from_str(cfg)?;

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
