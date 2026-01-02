use http::{Method, Request, Response, header::CONTENT_TYPE};
use qmt_openai::api::{
    OpenAIProviderConfig, openai_chat_request, openai_embed_request, openai_parse_chat,
    openai_parse_embed, url_schema,
};
use querymt::{
    HTTPLLMProvider,
    chat::{
        ChatMessage, ChatResponse, StructuredOutputFormat, Tool, ToolChoice, http::HTTPChatProvider,
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    get_env_var,
    plugin::HTTPLLMProviderFactory,
    providers::{ModelPricing, ProvidersRegistry},
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Alibaba {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Alibaba::default_base_url")]
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
    pub thinking_budget: Option<u32>,
}

impl OpenAIProviderConfig for Alibaba {
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

    fn json_schema(&self) -> Option<&StructuredOutputFormat> {
        self.json_schema.as_ref()
    }

    fn extra_body(&self) -> Option<serde_json::Map<String, Value>> {
        if let Some(thinking_budget) = self.thinking_budget {
            let mut map = Map::new();
            map.insert("thinking_budget".into(), thinking_budget.into());
            map.insert("enable_thinking".into(), true.into());
            return Some(map);
        }
        None
    }
}

impl HTTPChatProvider for Alibaba {
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

impl HTTPEmbeddingProvider for Alibaba {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for Alibaba {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        !unimplemented!("feature is missing!")
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        !unimplemented!("feature is missing!")
    }
}

impl HTTPLLMProvider for Alibaba {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl Alibaba {
    fn default_base_url() -> Url {
        Url::parse("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/").unwrap()
    }
}

struct AlibabaFactory;

impl HTTPLLMProviderFactory for AlibabaFactory {
    fn name(&self) -> &str {
        "alibaba"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("ALIBABA_API_KEY".into())
    }

    fn list_models_request(&self, _cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        Ok(Request::builder()
            .method(Method::GET)
            .uri(Alibaba::default_base_url().as_str().to_string())
            .header(CONTENT_TYPE, "application/json")
            .body(Vec::new())?)
    }

    fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Ok(vec![
            "qwq-plus",
            "qwen-max",
            "qwen-max-latest",
            "qwen-max-2025-01-25",
            "qwen-plus",
            "qwen-plus-latest",
            "qwen-plus-2025-01-25",
            "qwen-plus-2025-04-28",
            "qwen-turbo",
            "qwen-turbo-latest",
            "qwen-turbo-2024-11-01",
            "qwen-turbo-2025-04-28",
            "qwen3-235b-a22b",
            "qwen3-32b",
            "qwen3-30b-a3b",
            "qwen3-14b",
            "qwen3-8b",
            "qwen3-4b",
            "qwen3-1.7b",
            "qwen3-0.6b",
            "qwen2.5-14b-instruct-1m",
            "qwen2.5-7b-instruct-1m",
            "qwen2.5-72b-instruct",
            "qwen2.5-32b-instruct",
            "qwen2.5-14b-instruct",
            "qwen2.5-7b-instruct",
            "qwen2-72b-instruct",
            "qwen2-57b-a14b-instruct",
            "qwen2-7b-instruct",
            "qwen1.5-110b-chat",
            "qwen1.5-72b-chat",
            "qwen1.5-32b-chat",
            "qwen1.5-14b-chat",
            "qwen1.5-7b-chat",
            "qwen2.5-omni-7b",
            "qvq-max",
            "qvq-max-latest",
            "qvq-max-2025-03-25",
        ]
        .into_iter()
        .map(String::from)
        .collect())
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Alibaba);
        // Extract the schema object and turn it into a serde_json::Value
        serde_json::to_value(&schema.schema)
            .expect("OpenRouter JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Alibaba = serde_json::from_value(cfg.clone())?;

        Ok(Box::new(provider))
    }
}

#[warn(dead_code)]
fn get_pricing(model: &str) -> Option<ModelPricing> {
    if let Some(models) = get_env_var!("PROVIDERS_REGISTRY_DATA") {
        if let Ok(registry) = serde_json::from_str::<ProvidersRegistry>(&models) {
            return registry.get_pricing("alibaba", model).cloned();
        }
    }
    None
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(AlibabaFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Alibaba, AlibabaFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Alibaba,
        factory = AlibabaFactory,
        name   = "alibaba",
    }
}
