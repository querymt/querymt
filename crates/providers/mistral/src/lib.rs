use http::{Request, Response};
use qmt_openai::api::{
    openai_chat_request, openai_embed_request, openai_list_models_request, openai_parse_chat,
    openai_parse_embed, openai_parse_list_models, url_schema, OpenAIProviderConfig,
};
use querymt::{
    chat::{
        http::HTTPChatProvider, ChatMessage, ChatResponse, StructuredOutputFormat, Tool, ToolChoice,
    },
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
pub struct Mistral {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Mistral::default_base_url")]
    pub base_url: Url,
    pub api_key: String,
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

impl OpenAIProviderConfig for Mistral {
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

impl HTTPChatProvider for Mistral {
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

impl HTTPEmbeddingProvider for Mistral {
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

impl HTTPCompletionProvider for Mistral {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error>> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for Mistral {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl Mistral {
    fn default_base_url() -> Url {
        Url::parse("https://api.mistral.ai/v1/").unwrap()
    }
}

struct MistralFactory;

impl HTTPLLMProviderFactory for MistralFactory {
    fn name(&self) -> &str {
        "mistral"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("MISTRAL_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => Mistral::default_base_url(),
        };
        openai_list_models_request(&base_url, cfg)
    }

    fn parse_list_models(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Mistral);
        serde_json::to_value(&schema.schema).expect("Mistral JSON Schema should always serialize")
    }

    fn from_config(
        &self,
        cfg: &Value,
    ) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn std::error::Error>> {
        let provider: Mistral = serde_json::from_value(cfg.clone())
            .map_err(|e| LLMError::PluginError(format!("Mistral config error: {}", e)))?;

        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(MistralFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Mistral, MistralFactory};
    use querymt::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Mistral,
        factory = MistralFactory,
        name   = "mistral",
    }
}
