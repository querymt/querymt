//! Codex (ChatGPT backend) provider for QueryMT.

use http::{Request, Response};
use querymt::{
    HTTPLLMProvider,
    auth::ApiKeyResolver,
    chat::{
        ChatMessage, ChatOutput, StreamChunk, Tool, ToolChoice,
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
    fn classify_chat_error(&self, response: &Response<Vec<u8>>) -> LLMError {
        api::classify_codex_http_error(response)
    }

    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        api::codex_chat_request(self, messages, tools)
    }

    fn parse_chat(&self, response: Response<Vec<u8>>) -> Result<ChatOutput, LLMError> {
        let tool_state_buffer = Arc::new(Mutex::new(HashMap::new()));
        api::codex_parse_chat_with_state(response, &tool_state_buffer)
    }

    fn chat_stream_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        api::codex_chat_request(self, messages, tools)
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
    state: qmt_openai::api::OpenAIResponsesStreamState,
    policy: qmt_openai::api::ResponsesStreamPolicy,
    initialized: bool,
    completed: bool,
}

impl ChatStreamParser for CodexStreamParser {
    fn parse_chunk(&mut self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        if !self.initialized {
            self.state =
                qmt_openai::api::OpenAIResponsesStreamState::for_provider(codex_provenance());
            self.policy.reject_end_turn_false = true;
            self.initialized = true;
        }
        let chunks = qmt_openai::api::parse_openai_responses_sse_chunk_with_policy(
            chunk,
            &mut self.state,
            &self.policy,
        )?;
        if chunks.iter().any(|chunk| {
            matches!(
                chunk,
                StreamChunk::Structured(
                    querymt::chat::StructuredStreamEvent::ResponseTerminal { .. }
                )
            )
        }) {
            self.completed = true;
        }
        Ok(chunks)
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, LLMError> {
        if self.completed {
            Ok(Vec::new())
        } else {
            Err(api::codex_stream_closed_error())
        }
    }
}

/// Provenance recorded for Codex-originated structured output.
fn codex_provenance() -> qmt_openai::api::ResponsesCodecProvider {
    qmt_openai::api::ResponsesCodecProvider::with_endpoint(
        "codex",
        "responses",
        api::codex_responses_endpoint(),
    )
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
        "gpt-5.3-codex-spark".to_string(),
        "gpt-5.4".to_string(),
        "gpt-5.4-mini".to_string(),
        "gpt-5.5".to_string(),
        "gpt-5.6-luna".to_string(),
        "gpt-5.6-terra".to_string(),
        "gpt-5.6-sol".to_string(),
        "gpt-6-astra".to_string(),
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

#[cfg(test)]
mod stream_parser_tests {
    use super::{ChatStreamParser, CodexStreamParser, LLMError, StreamChunk};
    use querymt::chat::{ChatOutputStatus, StructuredStreamEvent};
    use querymt::error::ProviderErrorKind;

    #[test]
    fn premature_eof_before_response_completed_is_retryable() {
        let mut parser = CodexStreamParser::default();
        let events = parser.parse_chunk(b"data: [DONE]\n\n").unwrap();
        assert!(events.is_empty());

        let error = parser.finish().unwrap_err();
        assert!(matches!(
            error,
            LLMError::ProviderResponseError(ref failure)
                if failure.kind() == ProviderErrorKind::UnknownTransient
                    && failure.message() == "stream closed before response.completed"
        ));
    }

    #[test]
    fn eof_after_response_completed_is_clean() {
        let mut parser = CodexStreamParser::default();
        let events = parser
            .parse_chunk(
                br#"data: {"type":"response.completed","response":{"id":"resp_1"}}

"#,
            )
            .unwrap();
        // The shared item-aware codec emits a semantic terminal rather than a
        // flattened legacy `Done` chunk, so structured continuation survives.
        assert!(events.iter().any(|event| matches!(
            event,
            StreamChunk::Structured(StructuredStreamEvent::ResponseTerminal {
                status: ChatOutputStatus::Completed,
                ..
            })
        )));
        assert!(parser.finish().unwrap().is_empty());
    }

    /// A streamed Codex response must retain native continuation state
    /// (encrypted reasoning, item IDs) as structured events rather than being
    /// flattened into lossy legacy chunks.
    #[test]
    fn codex_stream_retains_native_continuation_state() {
        use querymt::chat::ChatOutputItem;

        let mut parser = CodexStreamParser::default();
        let mut events = Vec::new();
        for payload in [
            r#"{"type":"response.created","response":{"id":"resp_nat","model":"gpt-5.1-codex"}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_nat","summary":[]}}"#,
            r#"{"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0,"part":{"type":"summary_text","text":""}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"native thought"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_nat","encrypted_content":"codex-encrypted-payload","summary":[{"type":"summary_text","text":"native thought"}]}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_nat","call_id":"call_nat","name":"lookup","arguments":""}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"q\": 1 }"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_nat","call_id":"call_nat","name":"lookup","arguments":"{\"q\": 1 }"}}"#,
            r#"{"type":"response.completed","response":{"id":"resp_nat","end_turn":true}}"#,
        ] {
            events.extend(
                parser
                    .parse_chunk(format!("data: {payload}\n\n").as_bytes())
                    .unwrap(),
            );
        }

        // The reasoning item completed with its encrypted continuation intact.
        let reasoning = events.iter().find_map(|event| match event {
            StreamChunk::Structured(StructuredStreamEvent::ItemCompleted { item, .. }) => {
                match item {
                    ChatOutputItem::Reasoning(reasoning) => Some(reasoning),
                    _ => None,
                }
            }
            _ => None,
        });
        let reasoning = reasoning.expect("reasoning item completion emitted");
        assert_eq!(reasoning.id.as_deref(), Some("rs_nat"));
        assert_eq!(
            reasoning.encrypted_content.as_deref(),
            Some("codex-encrypted-payload"),
            "encrypted reasoning must survive the Codex stream"
        );

        // Item IDs and byte-exact arguments survive for the function call.
        let call = events.iter().find_map(|event| match event {
            StreamChunk::Structured(StructuredStreamEvent::ItemCompleted { item, .. }) => {
                match item {
                    ChatOutputItem::FunctionCall(call) => Some(call),
                    _ => None,
                }
            }
            _ => None,
        });
        let call = call.expect("function call item completion emitted");
        assert_eq!(call.item_id.as_deref(), Some("fc_nat"));
        assert_eq!(call.arguments, r#"{"q": 1 }"#);

        // The reasoning summary delta was accumulated into the item.
        let has_summary = events.iter().any(|event| {
            matches!(
                event,
                StreamChunk::Structured(StructuredStreamEvent::ReasoningPartDelta { .. })
            )
        });
        assert!(
            has_summary,
            "reasoning deltas are emitted as structured events"
        );
    }
}
