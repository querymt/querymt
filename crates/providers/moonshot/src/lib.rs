use http::{Request, Response};
use qmt_openai::api::{
    OpenAIProviderConfig, openai_chat_request, openai_list_models_request, openai_parse_chat,
    openai_parse_list_models, url_schema,
};
use querymt::{
    HTTPLLMProvider,
    chat::{
        ChatMessage, ChatResponse, StructuredOutputFormat, Tool, ToolChoice, http::HTTPChatProvider,
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MoonshotAI {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "MoonshotAI::default_base_url")]
    pub base_url: Url,
    pub api_key: String,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
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
}

impl OpenAIProviderConfig for MoonshotAI {
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

impl HTTPChatProvider for MoonshotAI {
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
}

impl HTTPEmbeddingProvider for MoonshotAI {
    fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPCompletionProvider for MoonshotAI {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for MoonshotAI {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl MoonshotAI {
    fn default_base_url() -> Url {
        Url::parse("https://api.moonshot.ai/v1/").unwrap()
    }
}

struct MoonshotAIFactory;

impl HTTPLLMProviderFactory for MoonshotAIFactory {
    fn name(&self) -> &str {
        "moonshotai"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("MOONSHOT_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => MoonshotAI::default_base_url(),
        };
        openai_list_models_request(&base_url, cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(MoonshotAI);
        serde_json::to_value(&schema.schema)
            .expect("MoonshotAI JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: MoonshotAI = serde_json::from_value(cfg.clone())?;

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
    use super::{MoonshotAI, MoonshotAIFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = MoonshotAI,
        factory = MoonshotAIFactory,
        name   = "moonshotai",
    }
}

#[allow(dead_code)]
fn get_pricing(model: &str) -> Option<querymt::providers::ModelPricing> {
    use querymt::get_env_var;
    use querymt::providers::ProvidersRegistry;

    if let Some(models) = get_env_var!("PROVIDERS_REGISTRY_DATA")
        && let Ok(registry) = serde_json::from_str::<ProvidersRegistry>(&models)
    {
        return registry.get_pricing("moonshotai", model).cloned();
    }
    None
}
