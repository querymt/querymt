//! Ollama API client implementation for chat and completion functionality.
//!
//! This module provides integration with Ollama's local LLM server through its API.

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use http::{Method, Request, Response, header::AUTHORIZATION, header::CONTENT_TYPE};
use querymt::{
    FunctionCall, HTTPLLMProvider, ToolCall, Usage,
    chat::{
        ChatMessage, ChatResponse, ChatRole, Content, FinishReason, StructuredOutputFormat, Tool,
        http::HTTPChatProvider,
    },
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    get_env_var, handle_http_error,
    plugin::HTTPLLMProviderFactory,
};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use url::Url;

pub fn url_schema(_gen: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "format": "uri"
    })
}

/// Client for interacting with Ollama's API.
///
/// Provides methods for chat and completion requests using Ollama's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Ollama {
    // ===== Core Configuration =====
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Ollama::default_base_url")]
    pub base_url: Url,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout_seconds: Option<u64>,
    pub stream: Option<bool>,
    pub reasoning: Option<bool>,
    #[serde(
        default,
        deserialize_with = "querymt::params::deserialize_system_string"
    )]
    pub system: Option<String>,
    /// JSON schema for structured output
    pub json_schema: Option<StructuredOutputFormat>,
    /// Available tools for function calling
    pub tools: Option<Vec<Tool>>,

    // ===== Sampling & Generation Parameters =====
    /// Maximum tokens to generate (maps to num_predict in API)
    pub max_tokens: Option<u32>,

    /// Temperature controls randomness; higher values increase creativity
    pub temperature: Option<f32>,

    /// Top-K sampling; higher values increase diversity
    pub top_k: Option<u32>,

    /// Nucleus (Top-P) sampling probability
    pub top_p: Option<f32>,

    /// Minimum probability threshold for token selection
    pub min_p: Option<f32>,

    /// Typical probability; aims for quality and variety balance
    pub typical_p: Option<f32>,

    // ===== Repetition Control =====
    /// How far back to look for repetition prevention
    /// -1 = use num_ctx, 0 = disabled
    pub repeat_last_n: Option<i32>,

    /// Strength of repetition penalty; higher penalizes more
    pub repeat_penalty: Option<f32>,

    /// Penalty for token presence in output
    pub presence_penalty: Option<f32>,

    /// Penalty for token frequency in output
    pub frequency_penalty: Option<f32>,

    /// Whether to penalize newline tokens
    pub penalize_newline: Option<bool>,

    // ===== Generation Control =====
    /// Random seed for reproducible generation
    pub seed: Option<u32>,

    /// Sequences that will cause generation to stop
    pub stop: Option<Vec<String>>,

    /// Number of tokens to keep in context
    pub num_keep: Option<u32>,

    // ===== Performance Tuning =====
    /// Batch size for processing
    pub num_batch: Option<u32>,

    /// Number of CPU threads to use
    pub num_thread: Option<u32>,

    /// Number of GPU layers to offload to GPU
    pub num_gpu: Option<u32>,

    /// Primary GPU device ID
    pub main_gpu: Option<u32>,

    /// Whether to use memory mapping
    pub use_mmap: Option<bool>,

    /// Whether to use NUMA (Non-Uniform Memory Access)
    pub numa: Option<bool>,

    /// Sets the size of the context window used to generate the next token
    pub num_ctx: Option<u32>,
}

/// Request payload for Ollama's chat API endpoint.
#[derive(Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    think: bool,
    options: Option<OllamaOptions>,
    format: Option<OllamaResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
}

/// Ollama model parameters that can be set per-request
/// See: https://github.com/ollama/ollama/blob/main/docs/modelfile.mdx#valid-parameters-and-values
#[derive(Serialize, Clone)]
struct OllamaOptions {
    /// Sets the size of the context window used to generate the next token. (Default: 2048)
    #[serde(skip_serializing_if = "Option::is_none")]
    num_ctx: Option<u32>,

    /// Temperature controls randomness; higher values increase creativity. (Default: 0.8)
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,

    /// Top-K sampling; higher values increase diversity. (Default: 40)
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,

    /// Nucleus (Top-P) sampling probability. (Default: 0.9)
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,

    /// Minimum probability threshold for token selection. (Default: 0.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    min_p: Option<f32>,

    /// Typical probability; aims for quality and variety balance. (Default: 0.7)
    #[serde(skip_serializing_if = "Option::is_none")]
    typical_p: Option<f32>,

    /// How far back to look for repetition prevention. (-1 = num_ctx, 0 = disabled, Default: 64)
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat_last_n: Option<i32>,

    /// Strength of repetition penalty; higher penalizes more. (Default: 1.1)
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat_penalty: Option<f32>,

    /// Penalty for token presence in output. (Default: 0.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    presence_penalty: Option<f32>,

    /// Penalty for token frequency in output. (Default: 0.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_penalty: Option<f32>,

    /// Whether to penalize newline tokens. (Default: false)
    #[serde(skip_serializing_if = "Option::is_none")]
    penalize_newline: Option<bool>,

    /// Maximum number of tokens to predict. (-1 = infinite, Default: -1)
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i32>,

    /// Sequences that will cause generation to stop.
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,

    /// Random seed for reproducible generation. (Default: 0)
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u32>,

    /// Number of tokens to keep in context. (Default: 4)
    #[serde(skip_serializing_if = "Option::is_none")]
    num_keep: Option<u32>,

    /// Batch size for processing. (Default: 512)
    #[serde(skip_serializing_if = "Option::is_none")]
    num_batch: Option<u32>,

    /// Number of CPU threads to use. (Default: number of cores)
    #[serde(skip_serializing_if = "Option::is_none")]
    num_thread: Option<u32>,

    /// Number of GPU layers to offload to GPU. (Default: varies)
    #[serde(skip_serializing_if = "Option::is_none")]
    num_gpu: Option<u32>,

    /// Primary GPU device ID. (Default: 0)
    #[serde(skip_serializing_if = "Option::is_none")]
    main_gpu: Option<u32>,

    /// Whether to use memory mapping. (Default: true)
    #[serde(skip_serializing_if = "Option::is_none")]
    use_mmap: Option<bool>,

    /// Whether to use NUMA (Non-Uniform Memory Access). (Default: false)
    #[serde(skip_serializing_if = "Option::is_none")]
    numa: Option<bool>,
}

/// Individual message in an Ollama chat conversation.
#[derive(Serialize)]
struct OllamaChatMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
}

/// Response from Ollama's API endpoints.
#[derive(Deserialize, Debug)]
struct OllamaResponse {
    content: Option<String>,
    response: Option<String>,
    message: Option<OllamaChatResponseMessage>,
    done: bool,
    done_reason: Option<String>,
    prompt_eval_count: Option<u32>,
    eval_count: Option<u32>,
}

impl std::fmt::Display for OllamaResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let empty = String::new();
        let text = self
            .content
            .as_ref()
            .or(self.response.as_ref())
            .or(self.message.as_ref().map(|m| &m.content))
            .unwrap_or(&empty);

        if let Some(msg) = &self.message
            && let Some(tool_calls) = &msg.tool_calls
        {
            for tc in tool_calls {
                writeln!(
                    f,
                    "{{\"name\": \"{}\", \"arguments\": {}}}",
                    tc.function.name,
                    serde_json::to_string_pretty(&tc.function.arguments).unwrap_or_default()
                )?;
            }
        }
        write!(f, "{}", text)
    }
}

impl ChatResponse for OllamaResponse {
    fn text(&self) -> Option<String> {
        // FIXME: check empty string!
        self.content
            .as_ref()
            .or(self.response.as_ref())
            .or(self.message.as_ref().map(|m| &m.content))
            .map(|s| s.to_string())
    }

    fn tool_calls(&self) -> Option<Vec<ToolCall>> {
        let msg = self.message.as_ref()?;
        let calls = msg.tool_calls.as_ref()?;
        Some(
            calls
                .iter()
                .map(|otc| ToolCall {
                    id: format!("call_{}", otc.function.name),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: otc.function.name.clone(),
                        arguments: serde_json::to_string(&otc.function.arguments)
                            .unwrap_or_default(),
                    },
                })
                .collect(),
        )
    }

    fn usage(&self) -> Option<Usage> {
        self.prompt_eval_count.map(|input_tokens| Usage {
            input_tokens,
            output_tokens: self.eval_count.unwrap_or(0),
            ..Default::default()
        })
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        if self.done {
            // Check if there are tool calls - takes precedence over done_reason
            // because Ollama returns "stop" even when tool calls are present
            if self
                .message
                .as_ref()
                .and_then(|m| m.tool_calls.as_ref())
                .is_some_and(|tc| !tc.is_empty())
            {
                return Some(FinishReason::ToolCalls);
            }

            return Some(match self.done_reason.as_deref() {
                Some("stop") => FinishReason::Stop,
                Some("length") => FinishReason::Length,
                Some("unload" | "load") => FinishReason::Other,
                Some(_) | None => FinishReason::Unknown,
            });
        }
        None
    }
}

/// Message content within an Ollama chat API response.
#[derive(Deserialize, Debug)]
struct OllamaChatResponseMessage {
    content: String,
    tool_calls: Option<Vec<OllamaToolCall>>,
}

/// Request payload for Ollama's generate API endpoint.
#[derive(Serialize)]
struct OllamaGenerateRequest<'a> {
    model: String,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    suffix: Option<&'a str>,
    raw: bool,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

#[derive(Serialize)]
struct OllamaEmbeddingRequest {
    model: String,
    input: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct OllamaEmbeddingResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Deserialize, Debug, Serialize)]
#[serde(untagged)]
enum OllamaResponseType {
    #[serde(rename = "json")]
    Json,
    StructuredOutput(Value),
}

#[derive(Deserialize, Debug, Serialize)]
struct OllamaResponseFormat {
    #[serde(flatten)]
    format: OllamaResponseType,
}

#[derive(Deserialize, Debug)]
struct OllamaToolCall {
    function: OllamaFunctionToolCall,
}

/// Ollama's tool call response
#[derive(Deserialize, Debug)]
struct OllamaFunctionToolCall {
    /// Name of the tool that was called
    name: String,
    /// Arguments provided to the tool
    arguments: Value,
}

impl Ollama {
    fn default_base_url() -> Url {
        let base_url = get_env_var!("OLLAMA_HOST").unwrap_or("http://localhost:11434".to_string());
        Url::parse(&base_url).unwrap()
    }

    /// Add `Authorization: Bearer` header if `api_key` is set.
    fn maybe_add_auth(&self, builder: http::request::Builder) -> http::request::Builder {
        match &self.api_key {
            Some(key) if !key.is_empty() => {
                builder.header(AUTHORIZATION, format!("Bearer {}", key))
            }
            _ => builder,
        }
    }

    /// Builds OllamaOptions from Ollama configuration, handling all parameters
    fn build_options(&self) -> OllamaOptions {
        OllamaOptions {
            num_ctx: self.num_ctx,
            temperature: self.temperature,
            top_p: self.top_p,
            top_k: self.top_k,
            min_p: self.min_p,
            typical_p: self.typical_p,
            repeat_last_n: self.repeat_last_n,
            repeat_penalty: self.repeat_penalty,
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            penalize_newline: self.penalize_newline,
            num_predict: self.max_tokens.map(|t| t as i32),
            stop: self.stop.clone(),
            seed: self.seed,
            num_keep: self.num_keep,
            num_batch: self.num_batch,
            num_thread: self.num_thread,
            num_gpu: self.num_gpu,
            main_gpu: self.main_gpu,
            use_mmap: self.use_mmap,
            numa: self.numa,
        }
    }
}

impl HTTPChatProvider for Ollama {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut chat_messages: Vec<OllamaChatMessage> = vec![];

        for msg in messages {
            let role = match msg.role {
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
            }
            .to_string();

            let text = msg
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .collect::<Vec<_>>()
                .join("\n");

            let mut inline_images: Vec<String> = Vec::new();

            for block in &msg.content {
                match block {
                    Content::Image { data, .. } => inline_images.push(BASE64.encode(data)),
                    Content::ToolResult {
                        id, name, content, ..
                    } => {
                        let output = content
                            .iter()
                            .filter_map(|c| c.as_text())
                            .collect::<Vec<_>>()
                            .join("\n");
                        chat_messages.push(OllamaChatMessage {
                            role: "tool".to_string(),
                            name: name.clone(),
                            content: output,
                            images: None,
                        });

                        // If tool result contains images, emit a separate user image message
                        // because Ollama tool role only supports text content.
                        let tool_images: Vec<String> = content
                            .iter()
                            .filter_map(|c| match c {
                                Content::Image { data, .. } => Some(BASE64.encode(data)),
                                _ => None,
                            })
                            .collect();
                        if !tool_images.is_empty() {
                            chat_messages.push(OllamaChatMessage {
                                role: "user".to_string(),
                                name: None,
                                content: format!("[Tool result image for {id}]"),
                                images: Some(tool_images),
                            });
                        }
                    }
                    _ => {}
                }
            }

            let has_tool_result = msg.content.iter().any(|b| b.is_tool_result());
            if has_tool_result {
                // Tool results are already emitted above.
                // Keep non-empty text as normal role content to preserve context.
                if !text.is_empty() {
                    chat_messages.push(OllamaChatMessage {
                        role,
                        content: text,
                        images: None,
                        name: None,
                    });
                }
                continue;
            }

            if !text.is_empty() || !inline_images.is_empty() {
                chat_messages.push(OllamaChatMessage {
                    role,
                    content: text,
                    images: if inline_images.is_empty() {
                        None
                    } else {
                        Some(inline_images)
                    },
                    name: None,
                });
            }
        }

        if let Some(system) = &self.system {
            chat_messages.insert(
                0,
                OllamaChatMessage {
                    role: "system".to_string(),
                    content: system.clone(),
                    images: None,
                    name: None,
                },
            );
        }

        // Ollama doesn't require the "name" field in the schema, so we just use the schema itself
        let format = if let Some(schema) = &self.json_schema {
            schema.schema.as_ref().map(|schema| OllamaResponseFormat {
                format: OllamaResponseType::StructuredOutput(schema.clone()),
            })
        } else {
            None
        };

        let req_body = OllamaChatRequest {
            model: self.model.clone(),
            messages: chat_messages,
            stream: self.stream.unwrap_or(false),
            think: self.reasoning.unwrap_or(false),
            options: Some(self.build_options()),
            format,
            tools: tools.map(|t| t.to_vec()),
        };

        let req_json: Vec<u8> = serde_json::to_vec(&req_body)?;

        let url = self.base_url.join("api/chat")?;
        let builder = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json");
        Ok(self.maybe_add_auth(builder).body(req_json)?)
    }

    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        handle_http_error!(resp);

        let json_resp: OllamaResponse = serde_json::from_slice(resp.body())?;
        Ok(Box::new(json_resp))
    }
}

impl HTTPCompletionProvider for Ollama {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        let url = self.base_url.join("api/generate")?;

        let req_body = OllamaGenerateRequest {
            model: self.model.clone(),
            prompt: &req.prompt,
            suffix: req.suffix.as_deref(),
            raw: true,
            stream: false,
            options: Some(self.build_options()),
        };

        let builder = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json");
        Ok(self
            .maybe_add_auth(builder)
            .body(serde_json::to_vec(&req_body)?)?)
    }

    fn parse_complete(&self, resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        let ollama_response: OllamaResponse = serde_json::from_slice(resp.body())?;

        if let Some(prompt_response) = ollama_response.response {
            Ok(CompletionResponse {
                text: prompt_response,
            })
        } else {
            Err(LLMError::ProviderError(
                "No answer returned by Ollama".to_string(),
            ))
        }
    }
}

impl HTTPEmbeddingProvider for Ollama {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        let url = self.base_url.join("api/embed")?;

        let body = OllamaEmbeddingRequest {
            model: self.model.clone(),
            input: inputs.to_vec(),
        };

        let builder = Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json");
        Ok(self
            .maybe_add_auth(builder)
            .body(serde_json::to_vec(&body)?)?)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        let json_resp: OllamaEmbeddingResponse = serde_json::from_slice(resp.body())?;
        Ok(json_resp.embeddings)
    }
}

impl HTTPLLMProvider for Ollama {
    fn tools(&self) -> Option<&[Tool]> {
        self.tools.as_deref()
    }
}

struct OllamaFactory;
impl HTTPLLMProviderFactory for OllamaFactory {
    fn name(&self) -> &str {
        "ollama"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("OLLAMA_API_KEY".into())
    }

    fn supports_custom_models(&self) -> bool {
        true
    }

    fn list_models_request(&self, cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: Value = serde_json::from_str(cfg)?;
        let base = cfg
            .get("base_url")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| Ollama::default_base_url().as_str().to_string());
        let api_key = cfg.get("api_key").and_then(Value::as_str);

        let url: String = format!("{}/api/tags", base);
        let mut builder = Request::builder()
            .method(Method::GET)
            .header(CONTENT_TYPE, "application/json")
            .uri(url);
        if let Some(key) = api_key {
            if !key.is_empty() {
                builder = builder.header(AUTHORIZATION, format!("Bearer {}", key));
            }
        }
        Ok(builder.body(Vec::new())?)
    }

    fn parse_list_models(&self, resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        let resp_json: Value = serde_json::from_slice(resp.body())?;

        let arr = resp_json
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| LLMError::InvalidRequest("`models` missing or not an array".into()))?;

        let names = arr
            .iter()
            .filter_map(|m| m.get("model"))
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        Ok(names)
    }

    fn config_schema(&self) -> String {
        let schema = schema_for!(Ollama);
        serde_json::to_string(&schema).expect("Ollama JSON Schema should always serialize")
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let provider: Ollama = serde_json::from_str(cfg)?;
        Ok(Box::new(provider))
    }
}

/// Creates an Ollama HTTP factory for direct static registration.
pub fn create_http_factory() -> Arc<dyn HTTPLLMProviderFactory> {
    Arc::new(OllamaFactory)
}

#[cfg(feature = "native")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(OllamaFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Ollama, OllamaFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Ollama,
        factory = OllamaFactory,
        name   = "ollama",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::http::HTTPChatProvider;
    use querymt::completion::http::HTTPCompletionProvider;
    use querymt::embedding::http::HTTPEmbeddingProvider;

    #[test]
    fn factory_api_key_name_returns_ollama_api_key() {
        let factory = OllamaFactory;
        assert_eq!(factory.api_key_name(), Some("OLLAMA_API_KEY".to_string()),);
    }

    fn test_ollama(api_key: Option<&str>) -> Ollama {
        Ollama {
            base_url: Url::parse("http://localhost:11434").unwrap(),
            api_key: api_key.map(String::from),
            model: "llama3".to_string(),
            timeout_seconds: None,
            stream: Some(false),
            reasoning: None,
            system: None,
            json_schema: None,
            tools: None,
            max_tokens: None,
            temperature: None,
            top_k: None,
            top_p: None,
            min_p: None,
            typical_p: None,
            repeat_last_n: None,
            repeat_penalty: None,
            presence_penalty: None,
            frequency_penalty: None,
            penalize_newline: None,
            seed: None,
            stop: None,
            num_keep: None,
            num_batch: None,
            num_thread: None,
            num_gpu: None,
            main_gpu: None,
            use_mmap: None,
            numa: None,
            num_ctx: None,
        }
    }

    #[test]
    fn chat_request_includes_bearer_when_api_key_set() {
        let ollama = test_ollama(Some("test-key-123"));
        let req = ollama
            .chat_request(&[], None)
            .expect("chat_request should succeed");
        let auth = req
            .headers()
            .get("authorization")
            .expect("should have auth header");
        assert_eq!(auth.to_str().unwrap(), "Bearer test-key-123");
    }

    #[test]
    fn chat_request_omits_auth_when_no_api_key() {
        let ollama = test_ollama(None);
        let req = ollama
            .chat_request(&[], None)
            .expect("chat_request should succeed");
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn complete_request_includes_bearer_when_api_key_set() {
        let ollama = test_ollama(Some("key-abc"));
        let req = ollama
            .complete_request(&CompletionRequest {
                prompt: "hello".to_string(),
                suffix: None,
                max_tokens: None,
                temperature: None,
            })
            .expect("complete_request should succeed");
        let auth = req
            .headers()
            .get("authorization")
            .expect("should have auth header");
        assert_eq!(auth.to_str().unwrap(), "Bearer key-abc");
    }

    #[test]
    fn complete_request_omits_auth_when_no_api_key() {
        let ollama = test_ollama(None);
        let req = ollama
            .complete_request(&CompletionRequest {
                prompt: "hello".to_string(),
                suffix: None,
                max_tokens: None,
                temperature: None,
            })
            .expect("complete_request should succeed");
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn embed_request_includes_bearer_when_api_key_set() {
        let ollama = test_ollama(Some("embed-key"));
        let req = ollama
            .embed_request(&["test".to_string()])
            .expect("embed_request should succeed");
        let auth = req
            .headers()
            .get("authorization")
            .expect("should have auth header");
        assert_eq!(auth.to_str().unwrap(), "Bearer embed-key");
    }

    #[test]
    fn embed_request_omits_auth_when_no_api_key() {
        let ollama = test_ollama(None);
        let req = ollama
            .embed_request(&["test".to_string()])
            .expect("embed_request should succeed");
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn list_models_request_includes_bearer_when_api_key_in_config() {
        let factory = OllamaFactory;
        let cfg = r#"{"base_url":"http://localhost:11434","api_key":"list-key","model":"x"}"#;
        let req = factory
            .list_models_request(cfg)
            .expect("list_models_request should succeed");
        let auth = req
            .headers()
            .get("authorization")
            .expect("should have auth header");
        assert_eq!(auth.to_str().unwrap(), "Bearer list-key");
    }

    #[test]
    fn list_models_request_omits_auth_when_no_api_key_in_config() {
        let factory = OllamaFactory;
        let cfg = r#"{"base_url":"http://localhost:11434","model":"x"}"#;
        let req = factory
            .list_models_request(cfg)
            .expect("list_models_request should succeed");
        assert!(req.headers().get("authorization").is_none());
    }
}
