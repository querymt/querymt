//! OpenAI API client implementation for chat and completion functionality.
//!
//! This module provides integration with OpenAI's GPT models through their API.

use http::{response, Request, Response};
use querymt::{
    chat::{
        http::HTTPChatProvider, ChatMessage, ChatResponse, StructuredOutputFormat, Tool, ToolChoice,
    },
    completion::{http::HTTPCompletionProvider, CompletionRequest, CompletionResponse},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    get_env_var,
    plugin::HTTPLLMProviderFactory,
    pricing::{calculate_cost, ModelsPricingData, Pricing},
    HTTPLLMProvider,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// Client for interacting with OpenAI's API.
///
/// Provides methods for chat and completion requests using OpenAI's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct OpenAI {
    pub api_key: String,
    #[schemars(schema_with = "api::url_schema")]
    #[serde(default = "OpenAI::default_base_url")]
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
}

impl OpenAI {
    fn default_base_url() -> Url {
        Url::parse("https://api.openai.com/v1/").unwrap()
    }
}

pub mod api;

impl api::OpenAIProviderConfig for OpenAI {
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

    fn parse_chat(
        &self,
        response: Response<Vec<u8>>,
    ) -> Result<Box<dyn ChatResponse>, Box<dyn std::error::Error>> {
        // TODO: Cleanup before finish PR
        let q = api::openai_parse_chat(self, response.clone());
        let p = calculate_cost(
            q.unwrap().usage().unwrap(),
            get_pricing(&self.model).unwrap(),
        );
        println!("[openai calculated cost] -> {}", p);

        api::openai_parse_chat(self, response)
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
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => OpenAI::default_base_url(),
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
        let provider: OpenAI = serde_json::from_value(cfg.clone())?;
        Ok(Box::new(provider))
    }
}

fn get_pricing(model: &str) -> Option<Pricing> {
    if let Some(models) = get_env_var!("MODEL_PRICING_DATA") {
        if let Ok(models) = serde_json::from_str::<ModelsPricingData>(&models) {
            let model = match model {
                "gpt-3.5-0301" => "gpt-3.5-turbo",
                "gpt-3.5-turbo-16k-0613" => "gpt-3.5-turbo-16k",
                "gpt-4-0125-preview" => "gpt-4-turbo",
                "gpt-4-0613" => "gpt-4",
                "gpt-4-1106-preview" => "gpt-4-turbo",
                "gpt-4-1106-vision-preview" => "gpt-4-turbo",
                "gpt-4.1-2025-04-14" => "gpt-4.1",
                "gpt-4.1-mini-2025-04-14" => "gpt-4.1-mini",
                "gpt-4.1-nano-2025-04-14" => "gpt-4.1-nano",
                "gpt-4.5-preview-2025-02-27" => "gpt-4.5-preview",
                "gpt-4o-mini-search-preview-2025-03-11" => "gpt-4o-mini-search-preview",
                "gpt-4o-search-preview-2025-03-11" => "gpt-4o-search-preview",
                "gpt-4-turbo-2024-04-09" => "gpt-4-turbo",
                "o1-2024-12-17" => "o1",
                "o1-pro-2025-03-19" => "o1-pro",
                "o3-2025-04-16" => "o3",
                "o3-mini-2025-01-31" => "o3-mini",
                "o4-mini-2025-04-16" => "o4-mini",
                _ => model,
            };

            let model = format!("openai/{}", model);

            return models
                .data
                .iter()
                .find(|m| m.id == model)
                .map(|m| m.pricing.clone());
        }
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
