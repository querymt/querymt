//! DeepSeek API client implementation for chat and completion functionality.
//!
//! This module provides integration with DeepSeek's models through their API.

use http::{header::CONTENT_TYPE, Method, Request, Response};
use qmt_openai::api::{
    openai_chat_request, openai_embed_request, openai_list_models_request, openai_parse_chat,
    openai_parse_embed, openai_parse_list_models, url_schema, OpenAIProviderConfig,
};
use querymt::{
    chat::{http::HTTPChatProvider, ChatMessage, ChatResponse, Tool, ToolChoice},
    completion::{http::HTTPCompletionProvider, CompletionRequest, CompletionResponse},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
    HTTPLLMProvider,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Deepseek {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Deepseek::default_base_url")]
    pub base_url: Url,
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub tools: Option<Vec<Tool>>,
    pub tool_choice: Option<ToolChoice>,
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

    fn system(&self) -> Option<&str> {
        self.system.as_deref()
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
}

impl HTTPChatProvider for Deepseek {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        openai_chat_request(self, messages, tools)
    }

    fn parse_chat(
        &self,
        response: Response<Vec<u8>>,
    ) -> Result<Box<dyn ChatResponse>, Box<dyn std::error::Error>> {
        openai_parse_chat(self, response)
    }
}

impl HTTPEmbeddingProvider for Deepseek {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        openai_embed_request(self, inputs)
    }

    fn parse_embed(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for Deepseek {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(
        &self,
        _resp: Response<Vec<u8>>,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error>> {
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
        Url::parse("https://api.deepseek.com").unwrap()
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

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        openai_list_models_request(&Deepseek::default_base_url(), cfg)
    }

    fn parse_list_models(
        &self,
        response: Response<Vec<u8>>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        openai_parse_list_models(&response)
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Deepseek);
        // Extract the schema object and turn it into a serde_json::Value
        serde_json::to_value(&schema.schema).expect("Deepseek JSON Schema should always serialize")
    }

    fn from_config(
        &self,
        cfg: &Value,
    ) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn std::error::Error>> {
        let provider: Deepseek = serde_json::from_value(cfg.clone())
            .map_err(|e| LLMError::PluginError(format!("Deepseek config error: {}", e)))?;

        // 2) Done—our OpenAI::send/chat/etc methods will lazily build the Client
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(DeepseekFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Deepseek, DeepseekFactory};
    use querymt::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Deepseek,
        factory = DeepseekFactory,
        name   = "deepseek",
    }
}
