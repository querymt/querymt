//! Codex (ChatGPT backend) provider for QueryMT.

use http::{Method, Request, Response};
use querymt::{
    HTTPLLMProvider,
    chat::{ChatMessage, ChatResponse, StreamChunk, Tool, ToolChoice, http::HTTPChatProvider},
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    get_env_var,
    plugin::HTTPLLMProviderFactory,
    providers::{ModelPricing, ProvidersRegistry},
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

pub mod api;

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Codex {
    /// OAuth access token for ChatGPT/Codex backend.
    pub api_key: String,
    #[schemars(schema_with = "api::url_schema")]
    #[serde(default = "Codex::default_base_url")]
    pub base_url: Url,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Base instructions required by the Codex backend.
    pub instructions: Option<String>,
    #[serde(
        default,
        deserialize_with = "querymt::params::deserialize_system_string"
    )]
    pub system: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    /// Optional client version passed to the Codex models endpoint.
    pub client_version: Option<String>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    /// Internal buffer for streaming tool state (not serialized)
    #[serde(skip)]
    #[schemars(skip)]
    #[serde(default = "Codex::default_tool_state_buffer")]
    pub tool_state_buffer: Arc<Mutex<HashMap<usize, api::CodexToolUseState>>>,
}

impl Codex {
    fn default_base_url() -> Url {
        Url::parse("https://chatgpt.com/backend-api/codex/").unwrap()
    }

    fn default_tool_state_buffer() -> Arc<Mutex<HashMap<usize, api::CodexToolUseState>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }
}

impl api::CodexProviderConfig for Codex {
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

    fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
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

    fn client_version(&self) -> Option<&str> {
        self.client_version.as_deref()
    }
}

impl HTTPChatProvider for Codex {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        api::codex_chat_request(self, messages, tools)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        api::codex_parse_chat_with_state(response, &self.tool_state_buffer)
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn parse_chat_stream_chunk(&self, _chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        api::codex_parse_stream_chunk_with_state(_chunk, &self.tool_state_buffer)
    }
}

impl HTTPEmbeddingProvider for Codex {
    fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::ProviderError(
            "Embedding not supported for Codex backend".to_string(),
        ))
    }

    fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::ProviderError(
            "Embedding not supported for Codex backend".to_string(),
        ))
    }
}

impl HTTPCompletionProvider for Codex {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::ProviderError(
            "Completion not supported for Codex backend".to_string(),
        ))
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::ProviderError(
            "Completion not supported for Codex backend".to_string(),
        ))
    }
}

impl HTTPLLMProvider for Codex {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

struct CodexFactory;

impl HTTPLLMProviderFactory for CodexFactory {
    fn name(&self) -> &str {
        "codex"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("OPENAI_API_KEY".into())
    }

    fn list_models_request(&self, _cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        Ok(Request::builder()
            .method(Method::GET)
            .uri(Codex::default_base_url().as_str().to_string())
            .header("Content-Type", "application/json")
            .body(Vec::new())?)
    }

    fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Ok(vec![
            "gpt-5.1-codex-max".to_string(),
            "gpt-5.1-codex".to_string(),
            "gpt-5.1-codex-mini".to_string(),
            "gpt-5.2-codex".to_string(),
            "gpt-5.2".to_string(),
            "gpt-5.1".to_string(),
            "gpt-5-codex".to_string(),
            "gpt-5".to_string(),
            "gpt-5-codex-mini".to_string(),
            "codex-mini-latest".to_string(),
            "bengalfox".to_string(),
            "boomslang".to_string(),
        ])
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Codex);
        serde_json::to_value(&schema.schema).expect("Codex JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Codex = serde_json::from_value(cfg.clone())?;
        Ok(Box::new(provider))
    }
}

#[cfg(not(feature = "api"))]
#[warn(dead_code)]
fn get_pricing(model: &str) -> Option<ModelPricing> {
    if let Some(models) = get_env_var!("PROVIDERS_REGISTRY_DATA")
        && let Ok(registry) = serde_json::from_str::<ProvidersRegistry>(&models)
    {
        return registry.get_pricing("codex", model).cloned();
    }
    None
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(CodexFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Codex, CodexFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Codex,
        factory = CodexFactory,
        name   = "codex",
    }
}
