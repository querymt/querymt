use std::sync::Arc;

use futures::TryFutureExt;
use mistralrs::{
    ChatCompletionResponse, GgufModelBuilder, IsqType, Model, PagedAttentionMetaBuilder,
    RequestBuilder, RequestLike, Response, ResponseOk, TextMessageRole, TextMessages,
    TextModelBuilder,
};
use querymt::{
    chat::{
        BasicChatProvider, ChatMessage, ChatResponse, ChatRole, StructuredOutputFormat, Tool,
        ToolChatProvider, ToolChoice,
    },
    completion::{CompletionProvider, CompletionRequest, CompletionResponse},
    embedding::EmbeddingProvider,
    error::LLMError,
    plugin::LLMProviderFactory,
    LLMProvider,
};
use schemars::{schema_for, JsonSchema};
use serde::{de::value, Deserialize, Serialize};
use serde_json::Value;
use tokio;

#[derive(Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MistralRSConfig {
    pub model: String,
    pub tools: Option<Vec<Tool>>,
    // … any future JSON fields …
}

pub struct MistralRS {
    pub config: MistralRSConfig,

    /*
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
    */
    pub mrs_model: Box<Model>,
}

impl MistralRS {
    pub async fn new(cfg: MistralRSConfig) -> Result<Self, LLMError> {
        let m = TextModelBuilder::new(&cfg.model)
            //            .with_isq(IsqType::Q8_0)
            .with_logging()
            .build()
            .await
            .map_err(|e| LLMError::ProviderError(format!("{:#}", e)))?;

        let z = GgufModelBuilder::new("").with_logging();
        Ok(Self {
            config: cfg,
            mrs_model: Box::new(m),
        })
    }
}

#[derive(Debug, Deserialize)]
struct MistralChatResponse {
    text: Option<String>,
}

impl std::fmt::Display for MistralChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.text)
    }
}

impl From<ChatCompletionResponse> for MistralChatResponse {
    fn from(value: ChatCompletionResponse) -> Self {
        MistralChatResponse {
            text: value.choices[0].message.content.clone(),
        }
    }
}

impl ChatResponse for MistralChatResponse {
    fn text(&self) -> Option<String> {
        self.text.clone()
    }
    fn usage(&self) -> Option<querymt::Usage> {
        None
    }
    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        None
    }
    fn thinking(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl BasicChatProvider for MistralRS {
    async fn chat(&self, messages: &[ChatMessage]) -> Result<Box<dyn ChatResponse>, LLMError> {
        let mut req = RequestBuilder::new();
        for msg in messages {
            let role = match msg.role {
                ChatRole::User => TextMessageRole::User,
                ChatRole::Assistant => TextMessageRole::Assistant,
            };
            req = req.add_message(role, msg.content.clone());
        }
        //        println!("sending request {:?}", req);

        let response = self
            .mrs_model
            .send_chat_request(req)
            .map_err(|e| LLMError::InvalidRequest(format!("{:#}", e)))
            .await?;

        let x = MistralChatResponse::from(response);

        Ok(Box::new(x))
    }
}

#[async_trait::async_trait]
impl ToolChatProvider for MistralRS {}

#[async_trait::async_trait]
impl EmbeddingProvider for MistralRS {
    async fn embed(&self, input: Vec<String>) -> Result<Vec<Vec<f32>>, LLMError> {
        unimplemented!()
    }
}

#[async_trait::async_trait]
impl CompletionProvider for MistralRS {
    async fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, LLMError> {
        unimplemented!()
    }
}

impl LLMProvider for MistralRS {
    fn tools(&self) -> Option<&[Tool]> {
        //        self.tools.as_deref()
        None
    }
}

struct MistralRSFactory;

impl LLMProviderFactory for MistralRSFactory {
    fn name(&self) -> &str {
        "mistralrs"
    }

    fn config_schema(&self) -> Value {
        let schema = schema_for!(MistralRSConfig);
        // Extract the schema object and turn it into a serde_json::Value
        serde_json::to_value(&schema.schema)
            .expect("OpenRouter JSON Schema should always serialize")
    }

    fn list_models<'a>(
        &'a self,
        _cfg: &Value,
    ) -> querymt::plugin::Fut<'a, Result<Vec<String>, LLMError>> {
        unimplemented!()
    }

    fn from_config(&self, cfg: &Value) -> Result<Box<dyn LLMProvider>, LLMError> {
        let cfg: MistralRSConfig = serde_json::from_value(cfg.clone())
            .map_err(|e| LLMError::PluginError(format!("mistral.rs config error: {}", e)))?;

        let provider = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(MistralRS::new(cfg))
        })?;
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_factory() -> *mut dyn LLMProviderFactory {
    Box::into_raw(Box::new(MistralRSFactory)) as *mut _
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{BasicChatProvider, ChatMessageBuilder};
    use tokio;

    fn get_provider() -> Box<dyn LLMProvider> {
        let factory = MistralRSFactory {};
        let cfg = MistralRSConfig {
            model: "microsoft/Phi-3.5-mini-instruct".to_string(),
            tools: None,
        };

        let json_cfg = serde_json::to_value(cfg).unwrap();
        factory.from_config(&json_cfg).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn mrs_chat_integration_test() {
        let provider = get_provider();
        // build a simple conversation
        let messages = vec![ChatMessageBuilder::new(ChatRole::User)
            .content("Hello?")
            .build()];

        // call the trait
        let resp = provider.chat(&messages).await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "not implemented")]
    async fn embedding_provider_is_currently_unimplemented() {
        let provider = get_provider();
        // this hits `unimplemented!()` in MistralRS::embed
        let _ = provider.embed(vec!["foo".into()]).await.unwrap();
    }

    #[tokio::test]
    #[should_panic(expected = "not implemented")]
    async fn completion_provider_is_currently_unimplemented() {
        let provider = get_provider();
        let dummy_req = CompletionRequest {
            prompt: "test".into(),
            max_tokens: None,
            temperature: None,
            suffix: None,
        };
        let _ = provider.complete(&dummy_req).await.unwrap();
    }
}
