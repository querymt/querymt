//! Codex (ChatGPT backend) provider for QueryMT.

use http::{Request, Response};
use querymt::{
    HTTPLLMProvider,
    auth::ApiKeyResolver,
    chat::{
        ChatMessage, ChatResponse, StreamChunk, Tool, ToolChoice,
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
    pub reasoning_effort: Option<querymt::chat::ReasoningEffort>,
    /// Extra body fields to include in the API request (e.g. `store`, `promptCacheKey`).
    /// These are passed through as-is via `#[serde(flatten)]` in the request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, Value>>,

    /// Optional resolver for dynamic credential refresh (e.g., OAuth tokens).
    #[serde(skip)]
    #[schemars(skip)]
    pub key_resolver: Option<Arc<dyn ApiKeyResolver>>,
}

impl Codex {
    fn default_base_url() -> Url {
        Url::parse("https://chatgpt.com/backend-api/codex/").unwrap()
    }
}

impl api::CodexProviderConfig for Codex {
    fn api_key(&self) -> String {
        if let Some(ref resolver) = self.key_resolver {
            resolver.current()
        } else {
            self.api_key.clone()
        }
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

    fn reasoning_effort(&self) -> Option<querymt::chat::ReasoningEffort> {
        self.reasoning_effort
    }

    fn extra_body(&self) -> Option<serde_json::Map<String, Value>> {
        self.extra_body.clone()
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
        let tool_state_buffer = Arc::new(Mutex::new(HashMap::new()));
        api::codex_parse_chat_with_state(response, &tool_state_buffer)
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn chat_stream_parser(&self) -> Result<Box<dyn ChatStreamParser>, LLMError> {
        Ok(Box::new(CodexStreamParser::default()))
    }
}

#[derive(Default)]
struct CodexStreamParser {
    tool_states: Arc<Mutex<HashMap<usize, api::CodexToolUseState>>>,
}

impl ChatStreamParser for CodexStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        api::codex_parse_stream_chunk_with_state(chunk, &self.tool_states)
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

    fn key_resolver(&self) -> Option<&Arc<dyn ApiKeyResolver>> {
        self.key_resolver.as_ref()
    }

    fn set_key_resolver(&mut self, resolver: Arc<dyn ApiKeyResolver>) {
        self.key_resolver = Some(resolver);
    }
}

fn codex_models() -> Vec<String> {
    vec![
        "gpt-5.1-codex-max".to_string(),
        "gpt-5.1-codex".to_string(),
        "gpt-5.1-codex-mini".to_string(),
        "gpt-5.2-codex".to_string(),
        "gpt-5.3-codex".to_string(),
        "gpt-5.3-codex-spark".to_string(),
        "gpt-5.4".to_string(),
        "gpt-5.4-mini".to_string(),
        "gpt-5.5".to_string(),
        "gpt-5.2".to_string(),
        "gpt-5.1".to_string(),
        "gpt-5-codex".to_string(),
        "gpt-5".to_string(),
        "gpt-5-codex-mini".to_string(),
        "codex-mini-latest".to_string(),
        "bengalfox".to_string(),
        "boomslang".to_string(),
    ]
}

struct CodexFactory;

impl HTTPLLMProviderFactory for CodexFactory {
    fn name(&self) -> &str {
        "codex"
    }

    fn api_key_name(&self) -> Option<String> {
        None
    }

    fn list_models_static(&self, _cfg: &str) -> Option<Result<Vec<String>, LLMError>> {
        Some(Ok(codex_models()))
    }

    fn list_models_request(&self, _cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::NotImplemented(
            "Codex model list is static and does not require HTTP".to_string(),
        ))
    }

    fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Ok(codex_models())
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(Codex);
        serde_json::to_string(&schema).expect("Codex JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Codex = serde_json::from_str(cfg)?;
        Ok(Box::new(provider))
    }
}

/// Creates a Codex HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(CodexFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
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
