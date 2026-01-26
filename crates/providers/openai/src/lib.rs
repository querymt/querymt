//! OpenAI API client implementation for chat and completion functionality.
//!
//! This module provides integration with OpenAI's GPT models through their API.

use http::{Request, Response};
use querymt::{
    chat::{
        http::HTTPChatProvider, ChatMessage, ChatResponse, StreamChunk, StructuredOutputFormat,
        Tool, ToolChoice,
    },
    completion::{http::HTTPCompletionProvider, CompletionRequest, CompletionResponse},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    get_env_var,
    plugin::HTTPLLMProviderFactory,
    providers::{ModelPricing, ProvidersRegistry},
    stt, tts, HTTPLLMProvider,
};
use schemars::{schema_for, JsonSchema};
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

/// Authentication type for OpenAI API.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    /// Standard API key authentication (Bearer token).
    #[serde(rename = "api_key")]
    ApiKey,
    /// OAuth token authentication (Bearer token).
    #[serde(rename = "oauth")]
    OAuth,
    /// No authentication. No Authorization header is sent.
    ///
    /// Intended for OpenAI-compatible/self-hosted endpoints.
    #[serde(rename = "none")]
    NoAuth,
}

/// Client for interacting with OpenAI's API.
///
/// Provides methods for chat and completion requests using OpenAI's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OpenAI {
    #[serde(default)]
    pub api_key: String,
    /// Optional: Explicitly specify authentication type.
    /// This is only honored when the host is api.openai.com; other hosts always use API keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<AuthType>,
    #[schemars(schema_with = "api::url_schema")]
    #[serde(
        default = "OpenAI::default_base_url",
        deserialize_with = "deserialize_base_url"
    )]
    pub base_url: Url,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
    /// Embedding parameters
    pub embedding_encoding_format: Option<String>,
    pub embedding_dimensions: Option<u32>,
    pub reasoning_effort: Option<String>,
    /// JSON schema for structured output
    pub json_schema: Option<StructuredOutputFormat>,
    /// Internal buffer for streaming tool state (not serialized)
    #[serde(skip)]
    #[schemars(skip)]
    #[serde(default = "OpenAI::default_tool_state_buffer")]
    pub tool_state_buffer: Arc<Mutex<HashMap<usize, api::OpenAIToolUseState>>>,
}

impl OpenAI {
    fn default_base_url() -> Url {
        Url::parse("https://api.openai.com/v1/").unwrap()
    }

    fn default_tool_state_buffer() -> Arc<Mutex<HashMap<usize, api::OpenAIToolUseState>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }
}

pub mod api;

impl api::OpenAIProviderConfig for OpenAI {
    fn api_key(&self) -> &str {
        &self.api_key
    }

    fn auth_type(&self) -> Option<&AuthType> {
        self.auth_type.as_ref()
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

    fn reasoning_effort(&self) -> Option<&String> {
        self.reasoning_effort.as_ref()
    }

    fn json_schema(&self) -> Option<&StructuredOutputFormat> {
        self.json_schema.as_ref()
    }
}

impl HTTPChatProvider for OpenAI {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        api::openai_chat_request(self, messages, tools)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        api::openai_parse_chat(self, response)
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn parse_chat_stream_chunk(&self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        let mut tool_states = self.tool_state_buffer.lock().unwrap();
        api::parse_openai_sse_chunk(chunk, &mut tool_states)
    }
}

impl HTTPEmbeddingProvider for OpenAI {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        api::openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        api::openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for OpenAI {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for OpenAI {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }

    fn stt_request(&self, req: &stt::SttRequest) -> Result<Request<Vec<u8>>, LLMError> {
        api::openai_stt_request(self, req)
    }

    fn parse_stt(&self, resp: Response<Vec<u8>>) -> Result<stt::SttResponse, LLMError> {
        api::openai_parse_stt(self, resp)
    }

    fn tts_request(&self, req: &tts::TtsRequest) -> Result<Request<Vec<u8>>, LLMError> {
        api::openai_tts_request(self, req)
    }

    fn parse_tts(&self, resp: Response<Vec<u8>>) -> Result<tts::TtsResponse, LLMError> {
        api::openai_parse_tts(self, resp)
    }
}

struct OpenAIFactory;
impl HTTPLLMProviderFactory for OpenAIFactory {
    fn name(&self) -> &str {
        "openai"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("OPENAI_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => normalize_base_url(Url::parse(base_url_str)?),
            None => normalize_base_url(OpenAI::default_base_url()),
        };
        api::openai_list_models_request(&base_url, cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        api::openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(OpenAI);
        // Extract the schema object and turn it into a serde_json::Value
        serde_json::to_value(&schema.schema).expect("OpenAI JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let mut provider: OpenAI = serde_json::from_value(cfg.clone())?;
        provider.base_url = normalize_base_url(provider.base_url);
        Ok(Box::new(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::OpenAI;

    #[test]
    fn base_url_is_normalized_to_trailing_slash() {
        let cfg = serde_json::json!({
            "api_key": "",
            "base_url": "http://localhost:8000/v1",
            "model": "gpt-4o-mini"
        });
        let provider: OpenAI = serde_json::from_value(cfg).unwrap();
        assert_eq!(provider.base_url.as_str(), "http://localhost:8000/v1/");
        let joined = provider.base_url.join("audio/transcriptions").unwrap();
        assert_eq!(
            joined.as_str(),
            "http://localhost:8000/v1/audio/transcriptions"
        );
    }
}

#[cfg(not(feature = "api"))]
#[warn(dead_code)]
fn get_pricing(model: &str) -> Option<ModelPricing> {
    if let Some(models) = get_env_var!("PROVIDERS_REGISTRY_DATA")
        && let Ok(registry) = serde_json::from_str::<ProvidersRegistry>(&models)
    {
        return registry.get_pricing("openai", model).cloned();
    }
    None
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(OpenAIFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{OpenAI, OpenAIFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = OpenAI,
        factory = OpenAIFactory,
        name   = "openai",
    }
}
