use http::{Request, Response};
use qmt_openai::api::{
    OpenAIProviderConfig, OpenAIToolUseState, openai_chat_request, openai_embed_request,
    openai_list_models_request, openai_parse_chat, openai_parse_embed, openai_parse_list_models,
    parse_openai_sse_chunk, url_schema,
};
use querymt::{
    HTTPLLMProvider,
    chat::{
        ChatMessage, ChatResponse, StreamChunk, StructuredOutputFormat, Tool, ToolChoice,
        http::HTTPChatProvider,
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
use std::sync::{Arc, Mutex};
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

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Deepseek {
    #[schemars(schema_with = "url_schema")]
    #[serde(
        default = "Deepseek::default_base_url",
        deserialize_with = "deserialize_base_url"
    )]
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
    pub json_schema: Option<StructuredOutputFormat>,
    pub reasoning_effort: Option<querymt::chat::ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, Value>>,
    /// Internal buffer for streaming tool state (not serialized)
    #[serde(skip)]
    #[schemars(skip)]
    #[serde(default = "Deepseek::default_tool_state_buffer")]
    pub tool_state_buffer: Arc<Mutex<HashMap<usize, OpenAIToolUseState>>>,
}

impl OpenAIProviderConfig for Deepseek {
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
        None
    }

    fn embedding_dimensions(&self) -> Option<&u32> {
        None
    }

    fn reasoning_effort(&self) -> Option<querymt::chat::ReasoningEffort> {
        self.reasoning_effort
    }

    fn json_schema(&self) -> Option<&StructuredOutputFormat> {
        self.json_schema.as_ref()
    }

    fn extra_body(&self) -> Option<serde_json::Map<String, Value>> {
        // DeepSeek requires {"thinking": {"type": "enabled"}} in the request body
        // when reasoning_effort is set. Merge it with any user-supplied extra_body.
        let mut map = self.extra_body.clone();
        if self.reasoning_effort.is_some() {
            let thinking = serde_json::json!({"type": "enabled"});
            map.get_or_insert_with(serde_json::Map::default)
                .insert("thinking".to_string(), thinking);
        }
        map
    }
}

impl HTTPChatProvider for Deepseek {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        openai_chat_request(self, messages, tools)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        openai_parse_chat(self, response)
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn parse_chat_stream_chunk(&self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        let mut tool_states = self.tool_state_buffer.lock().unwrap();
        parse_openai_sse_chunk(chunk, &mut tool_states)
    }
}

impl HTTPEmbeddingProvider for Deepseek {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for Deepseek {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for Deepseek {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl Deepseek {
    fn default_base_url() -> Url {
        Url::parse("https://api.deepseek.com/").unwrap()
    }

    fn default_tool_state_buffer() -> Arc<Mutex<HashMap<usize, OpenAIToolUseState>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }
}

struct DeepseekFactory;

impl HTTPLLMProviderFactory for DeepseekFactory {
    fn name(&self) -> &str {
        "deepseek"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("DEEPSEEK_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => normalize_base_url(Url::parse(base_url_str)?),
            None => Deepseek::default_base_url(),
        };
        openai_list_models_request(&base_url, &cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(Deepseek);
        serde_json::to_string(&schema).expect("DeepSeek JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let mut provider: Deepseek = serde_json::from_str(cfg)?;
        provider.base_url = normalize_base_url(provider.base_url);
        Ok(Box::new(provider))
    }
}

/// Creates a DeepSeek HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(DeepseekFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(DeepseekFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Deepseek, DeepseekFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Deepseek,
        factory = DeepseekFactory,
        name   = "deepseek",
    }
}
