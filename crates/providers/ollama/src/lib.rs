//! Ollama API client implementation for chat and completion functionality.
//!
//! This module provides integration with Ollama's local LLM server through its API.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use http::{header::CONTENT_TYPE, Method, Request, Response};
use querymt::{
    chat::{
        http::HTTPChatProvider, ChatMessage, ChatResponse, ChatRole, MessageType,
        StructuredOutputFormat, Tool,
    },
    completion::{http::HTTPCompletionProvider, CompletionRequest, CompletionResponse},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    plugin::HTTPLLMProviderFactory,
    FunctionCall, HTTPLLMProvider, ToolCall, Usage,
};
use schemars::{
    gen::SchemaGenerator,
    schema::{InstanceType, Schema, SchemaObject, SingleOrVec},
    schema_for, JsonSchema,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

pub fn url_schema(_gen: &mut SchemaGenerator) -> Schema {
    Schema::Object(SchemaObject {
        metadata: None,
        // say "this is a string"
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
        // with the "uri" format
        format: Some("uri".to_string()),
        ..Default::default()
    })
}

/// Client for interacting with Ollama's API.
///
/// Provides methods for chat and completion requests using Ollama's models.
#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct Ollama {
    #[schemars(schema_with = "url_schema")]
    #[serde(default = "Ollama::default_base_url")]
    pub base_url: Url,
    pub api_key: Option<String>,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub stream: Option<bool>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    /// JSON schema for structured output
    pub json_schema: Option<StructuredOutputFormat>,
    /// Available tools for function calling
    pub tools: Option<Vec<Tool>>,
}

/// Request payload for Ollama's chat API endpoint.
#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: String,
    messages: Vec<OllamaChatMessage<'a>>,
    stream: bool,
    options: Option<OllamaOptions>,
    format: Option<OllamaResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
}

#[derive(Serialize)]
struct OllamaOptions {
    top_p: Option<f32>,
    top_k: Option<u32>,
}

/// Individual message in an Ollama chat conversation.
#[derive(Serialize)]
struct OllamaChatMessage<'a> {
    role: &'a str,
    content: &'a str,
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

        if let Some(msg) = &self.message {
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    writeln!(
                        f,
                        "{{\"name\": \"{}\", \"arguments\": {}}}",
                        tc.function.name,
                        serde_json::to_string_pretty(&tc.function.arguments).unwrap_or_default()
                    )?;
                }
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
        if let Some(input_tokens) = self.prompt_eval_count {
            Some(Usage {
                input_tokens,
                output_tokens: self.eval_count.unwrap_or(0),
            })
        } else {
            None
        }
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

#[cfg(feature = "extism")]
fn get_env_var(key: &str) -> Option<String> {
    match extism_pdk::config::get(key) {
        Ok(value) => value,
        _ => None,
    }
}

#[cfg(not(feature = "extism"))]
fn get_env_var(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

impl Ollama {
    fn default_base_url() -> Url {
        let base_url = get_env_var("OLLAMA_HOST").unwrap_or("http://localhost:11434".to_string());
        Url::parse(&base_url).unwrap()
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
            match &msg.message_type {
                MessageType::Text => chat_messages.push(OllamaChatMessage {
                    role: match msg.role {
                        ChatRole::User => "user",
                        ChatRole::Assistant => "assistant",
                    },
                    content: &msg.content,
                    images: None,
                    name: None,
                }),
                MessageType::ToolResult(results) => {
                    for tool_result in results {
                        chat_messages.push(OllamaChatMessage {
                            role: "tool",
                            name: Some(tool_result.function.name.clone()),
                            content: &tool_result.function.arguments,
                            images: None,
                        })
                    }
                }
                MessageType::Image((_mime_type, content)) => {
                    chat_messages.push(OllamaChatMessage {
                        role: match msg.role {
                            ChatRole::User => "user",
                            ChatRole::Assistant => "assistant",
                        },
                        content: &msg.content,
                        images: Some(vec![BASE64.encode(content)]), // FIXME: this actually should be collected into MessageType::Text
                        name: None,
                    })
                }
                _ => (),
            }
        }

        if let Some(system) = &self.system {
            chat_messages.insert(
                0,
                OllamaChatMessage {
                    role: "system",
                    content: system,
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
            options: Some(OllamaOptions {
                top_p: self.top_p,
                top_k: self.top_k,
            }),
            format,
            tools: tools.map(|t| t.to_vec()),
        };

        let req_json: Vec<u8> = serde_json::to_vec(&req_body)?;

        let url = self.base_url.join("api/chat")?;
        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(req_json)?)
    }

    fn parse_chat(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Box<dyn ChatResponse>, Box<dyn std::error::Error>> {
        if !resp.status().is_success() {
            let status = resp.status();
            let error_text: String = serde_json::to_string(resp.body())?;
            return Err(Box::new(LLMError::ResponseFormatError {
                message: format!("API returned error status: {}", status),
                raw_response: error_text,
            }));
        }
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
        };

        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&req_body)?) // TODO: complete_request should return Result<Request...>
            ?)
    }

    fn parse_complete(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<CompletionResponse, Box<dyn std::error::Error>> {
        let ollama_response: OllamaResponse = serde_json::from_slice(resp.body())?;

        if let Some(prompt_response) = ollama_response.response {
            Ok(CompletionResponse {
                text: prompt_response,
            })
        } else {
            Err(Box::new(LLMError::ProviderError(
                "No answer returned by Ollama".to_string(),
            )))
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

        Ok(Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body)?)?)
    }

    fn parse_embed(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
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

    fn list_models_request(&self, cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg = cfg.clone();
        let base = cfg
            .get("base_url")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| return Ollama::default_base_url().as_str().to_string());

        let url: String = format!("{}/api/tags", base);
        Ok(Request::builder()
            .method(Method::GET)
            .header(CONTENT_TYPE, "application/json")
            .uri(url)
            .body(Vec::new())?)
    }

    fn parse_list_models(
        &self,
        resp: Response<Vec<u8>>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let resp_json: Value = serde_json::from_slice(&resp.body())?;

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

    fn config_schema(&self) -> Value {
        let schema = schema_for!(Ollama);
        serde_json::to_value(&schema.schema).expect("Ollama JSON Schema should always serialize")
    }

    fn from_config(
        &self,
        cfg: &Value,
    ) -> Result<Box<dyn HTTPLLMProvider>, Box<dyn std::error::Error>> {
        let provider: Ollama = serde_json::from_value(cfg.clone())
            .map_err(|e| LLMError::PluginError(format!("Ollama config error: {}", e)))?;
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(OllamaFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{Ollama, OllamaFactory};
    use querymt::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = Ollama,
        factory = OllamaFactory,
        name   = "ollama",
    }
}
