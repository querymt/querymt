use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use http::{Request, Response};
use querymt::{
    HTTPLLMProvider, ToolCall,
    chat::{ChatMessage, ChatResponse, FinishReason, StreamChunk, Tool, http::HTTPChatProvider},
    completion::{CompletionRequest, CompletionResponse, http::HTTPCompletionProvider},
    embedding::http::HTTPEmbeddingProvider,
    error::LLMError,
    handle_http_error,
    plugin::HTTPLLMProviderFactory,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

fn normalize_base_url(mut url: Url) -> Url {
    if !url.path().ends_with('/') {
        let p = url.path().to_string();
        url.set_path(&(p + "/"));
    }
    url
}

fn deserialize_base_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let url = Url::deserialize(deserializer)?;
    Ok(normalize_base_url(url))
}

#[derive(Debug, Clone, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProxyConfig {
    #[schemars(schema_with = "url_schema")]
    #[serde(
        default = "ProxyConfig::default_base_url",
        deserialize_with = "deserialize_base_url"
    )]
    pub base_url: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "querymt::params::deserialize_system_string"
    )]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Map<String, Value>>,
    #[serde(skip)]
    #[schemars(skip)]
    #[serde(default = "ProxyConfig::default_tool_state_buffer")]
    tool_state_buffer: Arc<Mutex<HashMap<usize, ProxyToolUseState>>>,
}

fn url_schema(r#gen: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    <String>::json_schema(r#gen)
}

impl ProxyConfig {
    fn default_base_url() -> Url {
        Url::parse("http://127.0.0.1:8080/").expect("default proxy base_url should be valid")
    }

    fn default_tool_state_buffer() -> Arc<Mutex<HashMap<usize, ProxyToolUseState>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }
}

#[derive(Debug)]
struct ProxyChatResponse {
    text: String,
    finish_reason: Option<FinishReason>,
    tool_calls: Option<Vec<ToolCall>>,
    usage: Option<querymt::Usage>,
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxyServiceResponse {
    choices: Vec<ProxyServiceChoice>,
    #[serde(default)]
    usage: Option<querymt::Usage>,
}

#[derive(Debug, Deserialize)]
struct ProxyServiceChoice {
    message: ProxyServiceMessage,
    finish_reason: String,
}

#[derive(Debug, Deserialize)]
struct ProxyServiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "reasoning_content")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ProxyServiceCompletionResponse {
    choices: Vec<ProxyServiceCompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct ProxyServiceCompletionChoice {
    text: String,
    #[allow(dead_code)]
    finish_reason: String,
}

#[derive(Default, Debug)]
struct ProxyToolUseState {
    id: String,
    name: String,
    arguments_buffer: String,
    started: bool,
}

#[derive(Deserialize, Debug)]
struct ProxyStreamChunk {
    #[serde(default)]
    choices: Vec<ProxyStreamChoice>,
    #[serde(default)]
    usage: Option<querymt::Usage>,
}

#[derive(Deserialize, Debug)]
struct ProxyStreamChoice {
    delta: ProxyStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ProxyStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, rename = "reasoning_content")]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ProxyStreamToolCall>>,
}

#[derive(Deserialize, Debug)]
struct ProxyStreamToolCall {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    function: ProxyStreamFunction,
}

#[derive(Deserialize, Debug)]
struct ProxyStreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: String,
}

impl std::fmt::Display for ProxyChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.text)
    }
}

impl ChatResponse for ProxyChatResponse {
    fn text(&self) -> Option<String> {
        Some(self.text.clone())
    }

    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        self.tool_calls.clone()
    }

    fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason
    }

    fn thinking(&self) -> Option<String> {
        self.thinking.clone()
    }

    fn usage(&self) -> Option<querymt::Usage> {
        self.usage.clone()
    }
}

#[derive(Debug, Serialize)]
struct ProxyMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<ProxyContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ProxyToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ProxyContent {
    Text(String),
    Parts(Vec<ProxyContentPart>),
}

#[derive(Debug, Serialize)]
struct ProxyContentPart {
    #[serde(rename = "type")]
    part_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<ProxyImageUrl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<ProxyContentSource>,
}

#[derive(Debug, Serialize)]
struct ProxyImageUrl {
    url: String,
}

#[derive(Debug, Serialize)]
struct ProxyContentSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct ProxyToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ProxyToolFunction,
}

#[derive(Debug, Serialize)]
struct ProxyToolFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ProxyChatRequest {
    messages: Vec<ProxyMessage>,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
struct ProxyCompletionRequest {
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    suffix: Option<String>,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Serialize)]
struct ProxyEmbeddingsRequest {
    input: Vec<String>,
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    extra_body: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
struct ProxyServiceEmbeddingsResponse {
    data: Vec<ProxyServiceEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct ProxyServiceEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

impl ProxyConfig {
    fn build_request_body(&self, messages: &[ChatMessage]) -> Result<ProxyChatRequest, LLMError> {
        let mut mapped_messages = Vec::new();
        for msg in messages {
            match &msg.message_type {
                querymt::chat::MessageType::Text => {
                    let role = match msg.role {
                        querymt::chat::ChatRole::User => "user",
                        querymt::chat::ChatRole::Assistant => "assistant",
                    };

                    mapped_messages.push(ProxyMessage {
                        role: role.to_string(),
                        content: Some(ProxyContent::Text(msg.content.clone())),
                        tool_calls: None,
                        tool_call_id: None,
                        tool_name: None,
                    });
                }
                querymt::chat::MessageType::Image((image_mime, raw_bytes)) => {
                    let role = match msg.role {
                        querymt::chat::ChatRole::User => "user",
                        querymt::chat::ChatRole::Assistant => "assistant",
                    };

                    let mut parts = Vec::new();
                    if !msg.content.is_empty() {
                        parts.push(ProxyContentPart {
                            part_type: "text".to_string(),
                            text: Some(msg.content.clone()),
                            image_url: None,
                            source: None,
                        });
                    }
                    parts.push(ProxyContentPart {
                        part_type: "image".to_string(),
                        text: None,
                        image_url: None,
                        source: Some(ProxyContentSource {
                            source_type: "base64".to_string(),
                            media_type: image_mime.mime_type().to_string(),
                            data: BASE64.encode(raw_bytes),
                        }),
                    });

                    mapped_messages.push(ProxyMessage {
                        role: role.to_string(),
                        content: Some(ProxyContent::Parts(parts)),
                        tool_calls: None,
                        tool_call_id: None,
                        tool_name: None,
                    });
                }
                querymt::chat::MessageType::Pdf(raw_bytes) => {
                    let role = match msg.role {
                        querymt::chat::ChatRole::User => "user",
                        querymt::chat::ChatRole::Assistant => "assistant",
                    };

                    let mut parts = Vec::new();
                    if !msg.content.is_empty() {
                        parts.push(ProxyContentPart {
                            part_type: "text".to_string(),
                            text: Some(msg.content.clone()),
                            image_url: None,
                            source: None,
                        });
                    }
                    parts.push(ProxyContentPart {
                        part_type: "document".to_string(),
                        text: None,
                        image_url: None,
                        source: Some(ProxyContentSource {
                            source_type: "base64".to_string(),
                            media_type: "application/pdf".to_string(),
                            data: BASE64.encode(raw_bytes),
                        }),
                    });

                    mapped_messages.push(ProxyMessage {
                        role: role.to_string(),
                        content: Some(ProxyContent::Parts(parts)),
                        tool_calls: None,
                        tool_call_id: None,
                        tool_name: None,
                    });
                }
                querymt::chat::MessageType::ImageURL(url) => {
                    let role = match msg.role {
                        querymt::chat::ChatRole::User => "user",
                        querymt::chat::ChatRole::Assistant => "assistant",
                    };

                    let mut parts = Vec::new();
                    if !msg.content.is_empty() {
                        parts.push(ProxyContentPart {
                            part_type: "text".to_string(),
                            text: Some(msg.content.clone()),
                            image_url: None,
                            source: None,
                        });
                    }
                    parts.push(ProxyContentPart {
                        part_type: "image_url".to_string(),
                        text: None,
                        image_url: Some(ProxyImageUrl { url: url.clone() }),
                        source: None,
                    });

                    mapped_messages.push(ProxyMessage {
                        role: role.to_string(),
                        content: Some(ProxyContent::Parts(parts)),
                        tool_calls: None,
                        tool_call_id: None,
                        tool_name: None,
                    });
                }
                querymt::chat::MessageType::ToolUse(tool_calls) => {
                    let calls = tool_calls
                        .iter()
                        .map(|call| ProxyToolCall {
                            id: call.id.clone(),
                            call_type: call.call_type.clone(),
                            function: ProxyToolFunction {
                                name: call.function.name.clone(),
                                arguments: call.function.arguments.clone(),
                            },
                        })
                        .collect::<Vec<_>>();

                    mapped_messages.push(ProxyMessage {
                        role: "assistant".to_string(),
                        content: if msg.content.is_empty() {
                            None
                        } else {
                            Some(ProxyContent::Text(msg.content.clone()))
                        },
                        tool_calls: Some(calls),
                        tool_call_id: None,
                        tool_name: None,
                    });
                }
                querymt::chat::MessageType::ToolResult(results) => {
                    for result in results {
                        mapped_messages.push(ProxyMessage {
                            role: "tool".to_string(),
                            content: Some(ProxyContent::Text(result.function.arguments.clone())),
                            tool_calls: None,
                            tool_call_id: Some(result.id.clone()),
                            tool_name: Some(result.function.name.clone()),
                        });
                    }
                }
            }
        }

        let (provider_id, _) = self.model.split_once(':').ok_or_else(|| {
            LLMError::InvalidRequest(
                "Proxy model must include provider prefix (e.g. llama_cpp:...)".to_string(),
            )
        })?;
        ensure_allowed_provider(provider_id)?;

        Ok(ProxyChatRequest {
            messages: mapped_messages,
            model: self.model.clone(),
            system: self.system.clone(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            top_p: self.top_p,
            top_k: self.top_k,
            timeout_seconds: self.timeout_seconds,
            stream: self.stream,
            tools: None,
            extra_body: self.extra_body.clone(),
        })
    }
}

fn ensure_allowed_provider(provider_id: &str) -> Result<(), LLMError> {
    match provider_id {
        "llama_cpp" | "mrs" => Ok(()),
        _ => Err(LLMError::InvalidRequest(
            "Only llama_cpp and mrs providers are supported".to_string(),
        )),
    }
}

impl HTTPChatProvider for ProxyConfig {
    fn chat_request(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[Tool]>,
    ) -> Result<Request<Vec<u8>>, LLMError> {
        let mut body = self.build_request_body(messages)?;
        if let Some(tool_list) = tools {
            if !tool_list.is_empty() {
                body.tools = Some(tool_list.to_vec());
            }
        }
        let url = self
            .base_url
            .join("v1/chat/completions")
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
        let bytes =
            serde_json::to_vec(&body).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        let mut req = Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("content-type", "application/json")
            .body(bytes)
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        if let Some(key) = &self.api_key {
            let value = format!("Bearer {key}");
            req.headers_mut().insert(
                "authorization",
                value.parse().map_err(|e| {
                    LLMError::InvalidRequest(format!("Invalid API key header: {e}"))
                })?,
            );
        }

        Ok(req)
    }

    fn parse_chat(&self, resp: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
        handle_http_error!(resp);

        let body = resp.into_body();
        let parsed: ProxyServiceResponse =
            serde_json::from_slice(&body).map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let choice =
            parsed.choices.into_iter().next().ok_or_else(|| {
                LLMError::ProviderError("Proxy response missing choices".to_string())
            })?;

        let finish_reason = match choice.finish_reason.as_str() {
            "stop" => Some(FinishReason::Stop),
            "length" => Some(FinishReason::Length),
            "content_filter" => Some(FinishReason::ContentFilter),
            "tool_calls" => Some(FinishReason::ToolCalls),
            "error" => Some(FinishReason::Error),
            _ => Some(FinishReason::Other),
        };

        Ok(Box::new(ProxyChatResponse {
            text: choice.message.content.unwrap_or_default(),
            finish_reason,
            tool_calls: choice.message.tool_calls,
            usage: parsed.usage,
            thinking: choice.message.reasoning_content,
        }))
    }

    fn supports_streaming(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    fn parse_chat_stream_chunk(&self, chunk: &[u8]) -> Result<Vec<StreamChunk>, LLMError> {
        if chunk.is_empty() {
            return Ok(Vec::new());
        }

        let text = String::from_utf8_lossy(chunk);
        let mut results = Vec::new();
        let mut tool_states = self.tool_state_buffer.lock().unwrap();
        let mut done_emitted = false;

        for line in text.lines() {
            if done_emitted {
                break;
            }

            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let data = match line.strip_prefix("data: ") {
                Some(d) => d,
                None => continue,
            };

            if data == "[DONE]" {
                for (index, state) in tool_states.drain() {
                    if state.started {
                        results.push(StreamChunk::ToolUseComplete {
                            index,
                            tool_call: querymt::ToolCall {
                                id: state.id,
                                call_type: "function".to_string(),
                                function: querymt::FunctionCall {
                                    name: state.name,
                                    arguments: state.arguments_buffer,
                                },
                            },
                        });
                    }
                }
                results.push(StreamChunk::Done {
                    stop_reason: "end_turn".to_string(),
                });
                done_emitted = true;
                continue;
            }

            let stream_chunk: ProxyStreamChunk =
                serde_json::from_str(data).map_err(|e| LLMError::ResponseFormatError {
                    message: format!("Failed to parse proxy stream chunk: {e}"),
                    raw_response: data.to_string(),
                })?;

            for choice in &stream_chunk.choices {
                if let Some(content) = &choice.delta.content {
                    if !content.is_empty() {
                        results.push(StreamChunk::Text(content.clone()));
                    }
                }

                if let Some(thinking) = &choice.delta.reasoning_content {
                    if !thinking.is_empty() {
                        results.push(StreamChunk::Thinking(thinking.clone()));
                    }
                }

                if let Some(tool_calls) = &choice.delta.tool_calls {
                    for tc in tool_calls {
                        let index = tc.index.unwrap_or(0);
                        let state = tool_states.entry(index).or_default();

                        if let Some(id) = &tc.id {
                            state.id = id.clone();
                        }
                        if let Some(name) = &tc.function.name {
                            state.name = name.clone();
                            if !state.started {
                                state.started = true;
                                results.push(StreamChunk::ToolUseStart {
                                    index,
                                    id: state.id.clone(),
                                    name: state.name.clone(),
                                });
                            }
                        }

                        if !tc.function.arguments.is_empty() {
                            state.arguments_buffer.push_str(&tc.function.arguments);
                            results.push(StreamChunk::ToolUseInputDelta {
                                index,
                                partial_json: tc.function.arguments.clone(),
                            });
                        }
                    }
                }

                if let Some(finish_reason) = &choice.finish_reason {
                    for (index, state) in tool_states.drain() {
                        if state.started {
                            results.push(StreamChunk::ToolUseComplete {
                                index,
                                tool_call: querymt::ToolCall {
                                    id: state.id,
                                    call_type: "function".to_string(),
                                    function: querymt::FunctionCall {
                                        name: state.name,
                                        arguments: state.arguments_buffer,
                                    },
                                },
                            });
                        }
                    }
                    results.push(StreamChunk::Done {
                        stop_reason: finish_reason.clone(),
                    });
                    done_emitted = true;
                }
            }

            if let Some(usage) = stream_chunk.usage {
                results.push(StreamChunk::Usage(usage));
            }
        }

        Ok(results)
    }
}

impl HTTPCompletionProvider for ProxyConfig {
    fn complete_request(&self, req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        let (provider_id, _) = self.model.split_once(':').ok_or_else(|| {
            LLMError::InvalidRequest(
                "Proxy model must include provider prefix (e.g. llama_cpp:...)".to_string(),
            )
        })?;
        ensure_allowed_provider(provider_id)?;

        let body = ProxyCompletionRequest {
            prompt: req.prompt.clone(),
            suffix: req.suffix.clone(),
            model: self.model.clone(),
            temperature: req.temperature.or(self.temperature),
            max_tokens: req.max_tokens.or(self.max_tokens),
            timeout_seconds: self.timeout_seconds,
            extra_body: self.extra_body.clone(),
        };

        let url = self
            .base_url
            .join("v1/completions")
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
        let bytes =
            serde_json::to_vec(&body).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        let mut req = Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("content-type", "application/json")
            .body(bytes)
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        if let Some(key) = &self.api_key {
            let value = format!("Bearer {key}");
            req.headers_mut().insert(
                "authorization",
                value.parse().map_err(|e| {
                    LLMError::InvalidRequest(format!("Invalid API key header: {e}"))
                })?,
            );
        }

        Ok(req)
    }

    fn parse_complete(&self, resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        handle_http_error!(resp);

        let body = resp.into_body();
        let parsed: ProxyServiceCompletionResponse =
            serde_json::from_slice(&body).map_err(|e| LLMError::ProviderError(e.to_string()))?;

        let choice = parsed.choices.into_iter().next().ok_or_else(|| {
            LLMError::ProviderError("Proxy completion response missing choices".to_string())
        })?;

        Ok(CompletionResponse { text: choice.text })
    }
}

impl HTTPEmbeddingProvider for ProxyConfig {
    fn embed_request(&self, inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        let (provider_id, _) = self.model.split_once(':').ok_or_else(|| {
            LLMError::InvalidRequest(
                "Proxy model must include provider prefix (e.g. llama_cpp:...)".to_string(),
            )
        })?;
        ensure_allowed_provider(provider_id)?;

        let body = ProxyEmbeddingsRequest {
            input: inputs.to_vec(),
            model: self.model.clone(),
            timeout_seconds: self.timeout_seconds,
            extra_body: self.extra_body.clone(),
        };

        let url = self
            .base_url
            .join("v1/embeddings")
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
        let bytes =
            serde_json::to_vec(&body).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        let mut req = Request::builder()
            .method("POST")
            .uri(url.as_str())
            .header("content-type", "application/json")
            .body(bytes)
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        if let Some(key) = &self.api_key {
            let value = format!("Bearer {key}");
            req.headers_mut().insert(
                "authorization",
                value.parse().map_err(|e| {
                    LLMError::InvalidRequest(format!("Invalid API key header: {e}"))
                })?,
            );
        }

        Ok(req)
    }

    fn parse_embed(&self, resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        handle_http_error!(resp);

        let body = resp.into_body();
        let mut parsed: ProxyServiceEmbeddingsResponse =
            serde_json::from_slice(&body).map_err(|e| LLMError::ProviderError(e.to_string()))?;

        parsed.data.sort_by_key(|d| d.index);
        Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
    }
}

impl HTTPLLMProvider for ProxyConfig {}

#[derive(Default)]
pub struct ProxyFactory;

impl HTTPLLMProviderFactory for ProxyFactory {
    fn name(&self) -> &str {
        "proxy"
    }

    fn api_key_name(&self) -> Option<String> {
        Some("QMT_PROXY_API_KEY".to_string())
    }

    fn config_schema(&self) -> String {
        let schema = schemars::schema_for!(ProxyConfig);
        serde_json::to_string(&schema.schema).expect("Proxy JSON Schema should always serialize")
    }

    fn list_models_request(&self, _cfg: &str) -> Result<Request<Vec<u8>>, LLMError> {
        let cfg: ProxyConfig =
            serde_json::from_str(_cfg).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        let url = cfg
            .base_url
            .join("v1/models")
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        let mut req = Request::builder()
            .method("GET")
            .uri(url.as_str())
            .header("content-type", "application/json")
            .body(Vec::new())
            .map_err(|e| LLMError::InvalidRequest(e.to_string()))?;

        if let Some(key) = &cfg.api_key {
            let value = format!("Bearer {key}");
            req.headers_mut().insert(
                "authorization",
                value.parse().map_err(|e| {
                    LLMError::InvalidRequest(format!("Invalid API key header: {e}"))
                })?,
            );
        }

        Ok(req)
    }

    fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        handle_http_error!(_resp);
        let body = _resp.into_body();
        let resp_json: Value =
            serde_json::from_slice(&body).map_err(|e| LLMError::ProviderError(e.to_string()))?;
        let arr = resp_json
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| LLMError::InvalidRequest("`data` missing or not an array".into()))?;

        let names = arr
            .iter()
            .filter_map(|m| m.get("id"))
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();

        Ok(names)
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let proxy_cfg: ProxyConfig =
            serde_json::from_str(cfg).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
        Ok(Box::new(proxy_cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use querymt::chat::{ChatRole, ImageMime, MessageType};

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            base_url: Url::parse("http://localhost:8080/").unwrap(),
            api_key: None,
            model: "llama_cpp:test".to_string(),
            temperature: None,
            max_tokens: None,
            stream: None,
            system: None,
            top_p: None,
            top_k: None,
            timeout_seconds: None,
            extra_body: None,
            tool_state_buffer: ProxyConfig::default_tool_state_buffer(),
        }
    }

    #[test]
    fn chat_request_serializes_image_url_as_parts() {
        let c = cfg();
        let msgs = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::ImageURL("https://example.com/a.png".to_string()),
            content: "cap".to_string(),
            thinking: None,
            cache: None,
        }];

        let req = c.chat_request(&msgs, None).unwrap();
        let json: Value = serde_json::from_slice(req.body()).unwrap();
        let msg0 = &json["messages"][0];
        assert_eq!(msg0["role"], "user");
        assert!(msg0["content"].is_array());
        let parts = msg0["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "cap");
        assert_eq!(parts[1]["type"], "image_url");
        assert_eq!(parts[1]["image_url"]["url"], "https://example.com/a.png");
    }

    #[test]
    fn chat_request_serializes_inline_image_as_base64_source() {
        let c = cfg();
        let raw = b"img".to_vec();
        let expected = BASE64.encode(&raw);
        let msgs = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Image((ImageMime::PNG, raw)),
            content: "cap".to_string(),
            thinking: None,
            cache: None,
        }];

        let req = c.chat_request(&msgs, None).unwrap();
        let json: Value = serde_json::from_slice(req.body()).unwrap();
        let parts = json["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "image");
        assert_eq!(parts[1]["source"]["type"], "base64");
        assert_eq!(parts[1]["source"]["media_type"], "image/png");
        assert_eq!(parts[1]["source"]["data"], expected);
    }

    #[test]
    fn chat_request_serializes_inline_pdf_as_document_base64_source() {
        let c = cfg();
        let raw = b"pdf".to_vec();
        let expected = BASE64.encode(&raw);
        let msgs = vec![ChatMessage {
            role: ChatRole::User,
            message_type: MessageType::Pdf(raw),
            content: "cap".to_string(),
            thinking: None,
            cache: None,
        }];

        let req = c.chat_request(&msgs, None).unwrap();
        let json: Value = serde_json::from_slice(req.body()).unwrap();
        let parts = json["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts[1]["type"], "document");
        assert_eq!(parts[1]["source"]["type"], "base64");
        assert_eq!(parts[1]["source"]["media_type"], "application/pdf");
        assert_eq!(parts[1]["source"]["data"], expected);
    }

    #[test]
    fn parse_chat_stream_chunk_emits_thinking_and_usage() {
        let c = cfg();
        let bytes = br#"data: {"choices":[{"delta":{"reasoning_content":"t"}}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0}}

"#;
        let out = c.parse_chat_stream_chunk(bytes).unwrap();

        assert!(
            out.iter()
                .any(|c| matches!(c, StreamChunk::Thinking(t) if t == "t"))
        );
        assert!(out.iter().any(|c| {
            matches!(
                c,
                StreamChunk::Usage(u) if u.input_tokens + u.output_tokens == 3
            )
        }));
    }

    #[test]
    fn parse_chat_parses_usage_field() {
        let c = cfg();
        let body = br#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"cache_read_tokens":0,"cache_write_tokens":0,"reasoning_tokens":0}}"#.to_vec();
        let resp = Response::builder().status(200).body(body).unwrap();
        let parsed = c.parse_chat(resp).unwrap();
        let usage = parsed.usage().unwrap();
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(usage.output_tokens, 2);
    }
}

#[cfg(feature = "native")]
#[no_mangle]
pub extern "C" fn plugin_http_factory() -> *mut dyn HTTPLLMProviderFactory {
    Box::into_raw(Box::new(ProxyFactory)) as *mut _
}

#[cfg(feature = "extism")]
mod extism_exports {
    use super::{ProxyConfig, ProxyFactory};
    use querymt_extism_macros::impl_extism_http_plugin;

    impl_extism_http_plugin! {
        config = ProxyConfig,
        factory = ProxyFactory,
        name   = "proxy",
    }
}
