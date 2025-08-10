use http::{
    header::{AUTHORIZATION, CONTENT_TYPE},
    Method, Request, Response,
};
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
    get_env_var,
    plugin::HTTPLLMProviderFactory,
    pricing::{ModelsPricingData, Pricing},
    HTTPLLMProvider, ToolCall,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Xai {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Xai::default_base_url")]
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

#[derive(Serialize)]
struct XaiCompletionRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    suffix: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<&'a u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<&'a f32>,
}

#[derive(Deserialize)]
struct XaiCompletionResponse {
    model: String,
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct ChatCompletionChoice {
    index: u32,
    message: AssistantMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct AssistantMessage {
    role: String,
    tool_calls: Option<Vec<ToolCall>>,
    content: String, //TODO: Either<String, Vec<String>>,
}

impl OpenAIProviderConfig for Xai {
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

impl HTTPChatProvider for Xai {
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
    ) -> Result<Box<dyn ChatResponse>, LLMError> {
        Ok(openai_parse_chat(self, response)?)
    }
}

impl HTTPEmbeddingProvider for Xai {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        openai_embed_request(self, inputs)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        openai_parse_embed(self, resp)
    }
}

impl HTTPCompletionProvider for Xai {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        let api_key = match self.api_key().into() {
            Some(key) => key,
            None => return Err(LLMError::AuthError("Missing API key".to_string())),
        };

        let body = XaiCompletionRequest {
            model: self.model(),
            prompt: &req.prompt,
            suffix: req.suffix.as_deref(),
            max_tokens: req.max_tokens.as_ref(),
            temperature: req.temperature.as_ref(),
        };

        let json_body = serde_json::to_vec(&body)?;
        let url = self
            .base_url()
            .join("fim/completions")
            .map_err(|e| LLMError::HttpError(e.to_string()))?;

        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.to_string())
            .header(AUTHORIZATION, format!("Bearer {}", api_key))
            .header(CONTENT_TYPE, "application/json")
            .body(json_body)?)
    }

    fn parse_complete(&self, resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        if !resp.status().is_success() {
            let status = resp.status();
            let error_text: String = serde_json::to_string(resp.body())?;
            return Err(LLMError::ResponseFormatError {
                message: format!("API returned error status: {}", status),
                raw_response: error_text,
            });
        }

        let json_resp: Result<XaiCompletionResponse, serde_json::Error> =
            serde_json::from_slice(&resp.body());
        match json_resp {
            Ok(completion_response) => Ok(CompletionResponse {
                text: completion_response.choices[0].message.content.clone(), // FIXME
            }),
            Err(e) => Err(LLMError::JsonError(e)),
        }
    }
}

impl HTTPLLMProvider for Xai {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

impl Xai {
    fn default_base_url() -> Url {
        Url::parse("https://api.x.ai/v1/").unwrap()
    }
}

struct XaiFactory;

impl HTTPLLMProviderFactory for XaiFactory {
    fn name(&self) -> &str {
        "xai"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("XAI_API_KEY".into())
    }

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        let base_url = match cfg.get("base_url").and_then(Value::as_str) {
            Some(base_url_str) => Url::parse(base_url_str)?,
            None => Xai::default_base_url(),
        };
        openai_list_models_request(&base_url, cfg)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        openai_parse_list_models(&resp)
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Xai);
        serde_json::to_value(&schema.schema).expect("Xai JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Xai = serde_json::from_value(cfg.clone())?;

        Ok(Box::new(provider))
    }
}

#[warn(dead_code)]
fn get_pricing(model: &str, thinking: bool) -> Option<Pricing> {
    // Source: https://docs.x.ai/docs/models
    if let Some(models) = get_env_var!("MODEL_PRICING_DATA") {
        if let Ok(models) = serde_json::from_str::<ModelsPricingData>(&models) {
            return match model {
                "grok-3-fast" | "grok-3-fast-latest" => Some(Pricing {
                    prompt: 0.000005,
                    completion: 0.000025,
                    request: 0.0,
                    image: 0.0,
                    web_search: 0.0,
                    internal_reasoning: 0.0,
                }),
                "grok-3-mini-fast" | "grok-3-mini-fast-latest" => Some(Pricing {
                    prompt: 0.0000006,
                    completion: 0.000004,
                    request: 0.0,
                    image: 0.0,
                    web_search: 0.0,
                    internal_reasoning: 0.0,
                }),

                _ => {
                    let remapped_model = match model {
                        "grok-3-latest" => "grok-3",
                        "grok-3-mini-latest" => "grok-3-mini",
                        "grok-2-vision" | "grok-2-vision-latest" => "grok-2-vision-1212",
                        "grok-2" | "grok-2-latest" => "grok-2-1212",
                        _ => model,
                    };
                    let model_id = format!("x-ai/{}", remapped_model);

                    models
                        .data
                        .iter()
                        .find(|m| m.id == model_id)
                        .map(|m| m.pricing.clone())
                }
            };
        }
    }
    None
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(XaiFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Xai, XaiFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Xai,
        factory = XaiFactory,
        name   = "xai",
    }
}
