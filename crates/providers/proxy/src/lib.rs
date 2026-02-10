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
}

#[derive(Debug, Deserialize)]
struct ProxyServiceResponse {
    choices: Vec<ProxyServiceChoice>,
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
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
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
    choices: Vec<ProxyStreamChoice>,
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

    fn usage(&self) -> Option<querymt::Usage> {
        None
    }
}

#[derive(Debug, Serialize)]
struct ProxyMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ProxyToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
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
                        content: Some(msg.content.clone()),
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
                            Some(msg.content.clone())
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
                            content: Some(result.function.arguments.clone()),
                            tool_calls: None,
                            tool_call_id: Some(result.id.clone()),
                            tool_name: Some(result.function.name.clone()),
                        });
                    }
                }
                _ => {
                    return Err(LLMError::InvalidRequest(
                        "Proxy provider only supports text and tool messages".to_string(),
                    ));
                }
            }
        }

        let target_provider = self
            .extra_body
            .as_ref()
            .and_then(|options| options.get("target_provider"));

        if let Some(target) = target_provider {
            if !matches!(target, Value::String(_)) {
                return Err(LLMError::InvalidRequest(
                    "target_provider must be a string".to_string(),
                ));
            }
            if let Value::String(provider_id) = target {
                ensure_allowed_provider(provider_id)?;
            }
        } else if !self.model.contains(':') {
            return Err(LLMError::InvalidRequest(
                "Proxy model must include provider prefix or set target_provider in extra_body"
                    .to_string(),
            ));
        } else if let Some((provider_id, _)) = self.model.split_once(':') {
            ensure_allowed_provider(provider_id)?;
        }

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
            let value = format!("Bearer {}", key);
            req.headers_mut().insert(
                "authorization",
                value.parse().map_err(|e| {
                    LLMError::InvalidRequest(format!("Invalid API key header: {}", e))
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
                    message: format!("Failed to parse proxy stream chunk: {}", e),
                    raw_response: data.to_string(),
                })?;

            for choice in &stream_chunk.choices {
                if let Some(content) = &choice.delta.content {
                    if !content.is_empty() {
                        results.push(StreamChunk::Text(content.clone()));
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
        }

        Ok(results)
    }
}

impl HTTPCompletionProvider for ProxyConfig {
    fn complete_request(&self, _req: &CompletionRequest) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::NotImplemented(
            "Completion not supported by proxy provider".to_string(),
        ))
    }

    fn parse_complete(&self, _resp: Response<Vec<u8>>) -> Result<CompletionResponse, LLMError> {
        Err(LLMError::NotImplemented(
            "Completion not supported by proxy provider".to_string(),
        ))
    }
}

impl HTTPEmbeddingProvider for ProxyConfig {
    fn embed_request(&self, _inputs: &[String]) -> Result<Request<Vec<u8>>, LLMError> {
        Err(LLMError::NotImplemented(
            "Embeddings not supported by proxy provider".to_string(),
        ))
    }

    fn parse_embed(&self, _resp: Response<Vec<u8>>) -> Result<Vec<Vec<f32>>, LLMError> {
        Err(LLMError::NotImplemented(
            "Embeddings not supported by proxy provider".to_string(),
        ))
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
        Err(LLMError::NotImplemented(
            "Model listing not supported by proxy provider".to_string(),
        ))
    }

    fn parse_list_models(&self, _resp: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
        Err(LLMError::NotImplemented(
            "Model listing not supported by proxy provider".to_string(),
        ))
    }

    fn from_config(&self, cfg: &str) -> Result<Box<dyn HTTPLLMProvider>, LLMError> {
        let proxy_cfg: ProxyConfig =
            serde_json::from_str(cfg).map_err(|e| LLMError::InvalidRequest(e.to_string()))?;
        Ok(Box::new(proxy_cfg))
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
