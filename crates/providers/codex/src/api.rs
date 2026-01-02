use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use http::{
    Method, Request, Response,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use querymt::{
    FunctionCall, ToolCall, Usage,
    chat::{ChatMessage, ChatResponse, ChatRole, MessageType, StreamChunk, Tool, ToolChoice},
    error::LLMError,
    handle_http_error,
};
use schemars::{
    r#gen::SchemaGenerator,
    schema::{InstanceType, Schema, SchemaObject, SingleOrVec},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use url::Url;

pub fn url_schema(_gen: &mut SchemaGenerator) -> Schema {
    Schema::Object(SchemaObject {
        metadata: None,
        instance_type: Some(SingleOrVec::Single(Box::new(InstanceType::String))),
        format: Some("uri".to_string()),
        ..Default::default()
    })
}

pub trait CodexProviderConfig {
    fn api_key(&self) -> &str;
    fn base_url(&self) -> &Url;
    fn model(&self) -> &str;
    fn max_tokens(&self) -> Option<&u32>;
    fn temperature(&self) -> Option<&f32>;
    fn instructions(&self) -> Option<&str>;
    fn system(&self) -> Option<&str>;
    fn timeout_seconds(&self) -> Option<&u64>;
    fn stream(&self) -> Option<&bool>;
    fn top_p(&self) -> Option<&f32>;
    fn top_k(&self) -> Option<&u32>;
    fn tools(&self) -> Option<&[Tool]>;
    fn tool_choice(&self) -> Option<&ToolChoice>;
    fn client_version(&self) -> Option<&str>;
}

#[derive(Debug, Clone, Default)]
pub struct CodexToolUseState {
    pub id: Option<String>,
    pub name: Option<String>,
    pub arguments: String,
    pub started: bool,
}

const INSTRUCTIONS_GPT_5_1_CODEX_MAX: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/instructions_gpt_5_1_codex_max.txt"
));
const INSTRUCTIONS_GPT_5_1_CODEX: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/instructions_gpt_5_1_codex.txt"
));
const INSTRUCTIONS_GPT_5_2: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/instructions_gpt_5_2.txt"
));
const INSTRUCTIONS_GPT_5_1: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/instructions_gpt_5_1.txt"
));
const INSTRUCTIONS_GPT_5: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/instructions_gpt_5.txt"
));
const DEFAULT_INSTRUCTIONS_DIRECTORY: &str = "querymt";

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
enum CodexInputItem<'a> {
    #[serde(rename = "message")]
    Message {
        role: &'a str,
        content: Vec<CodexInputContent<'a>>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: &'a str, output: &'a str },
}

#[derive(Serialize, Debug)]
struct CodexInputContent<'a> {
    #[serde(rename = "type")]
    content_type: &'a str,
    text: &'a str,
}

#[derive(Serialize, Debug)]
struct CodexChatRequest<'a> {
    model: &'a str,
    input: Vec<CodexInputItem<'a>>,
    instructions: &'a str,
    store: bool,
    #[serde(rename = "max_output_tokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<CodexTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
}

#[derive(Serialize, Debug)]
struct CodexTool<'a> {
    #[serde(rename = "type")]
    tool_type: &'a str,
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
    strict: bool,
}

#[derive(Deserialize, Debug)]
struct CodexChatResponse {
    output: Vec<CodexOutput>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Debug)]
struct CodexModelsResponse {
    models: Vec<CodexModelInfo>,
}

#[derive(Deserialize, Debug)]
struct CodexModelInfo {
    slug: String,
}

#[derive(Deserialize, Debug)]
struct CodexOutput {
    #[serde(rename = "type")]
    output_type: String,
    content: Option<Vec<CodexOutputContent>>,
}

#[derive(Deserialize, Debug)]
struct CodexOutputContent {
    #[serde(rename = "type")]
    content_type: String,
    text: Option<String>,
}

#[derive(Deserialize, Debug)]
struct CodexSseEvent {
    #[serde(rename = "type")]
    kind: String,
    delta: Option<String>,
    response: Option<Value>,
    item: Option<Value>,
    output_index: Option<usize>,
    item_id: Option<String>,
}

impl ChatResponse for CodexChatResponse {
    fn text(&self) -> Option<String> {
        let mut pieces = Vec::new();
        for output in &self.output {
            if output.output_type != "message" {
                continue;
            }
            if let Some(content) = &output.content {
                for item in content {
                    if item.content_type == "output_text" || item.content_type == "text" {
                        if let Some(text) = &item.text {
                            pieces.push(text.clone());
                        }
                    }
                }
            }
        }
        if pieces.is_empty() {
            None
        } else {
            Some(pieces.join(""))
        }
    }

    fn tool_calls(&self) -> Option<Vec<querymt::ToolCall>> {
        None
    }

    fn usage(&self) -> Option<Usage> {
        self.usage.clone()
    }
}

impl std::fmt::Display for CodexChatResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(text) = self.text() {
            write!(f, "{}", text)
        } else {
            write!(f, "")
        }
    }
}

pub fn codex_chat_request<C: CodexProviderConfig>(
    cfg: &C,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
) -> Result<Request<Vec<u8>>, LLMError> {
    let instructions = resolve_instructions(cfg.model(), cfg.instructions())?;
    let mut inputs = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = cfg.system().filter(|text| !text.trim().is_empty()) {
        let text = format!(
            "# AGENTS.md instructions for {directory}\n\n<INSTRUCTIONS>\n{system}\n</INSTRUCTIONS>",
            directory = DEFAULT_INSTRUCTIONS_DIRECTORY
        );
        let leaked = Box::leak(text.into_boxed_str());
        inputs.push(CodexInputItem::Message {
            role: "user",
            content: vec![CodexInputContent {
                content_type: "input_text",
                text: leaked,
            }],
        });
    }
    for msg in messages {
        let (role, content_type) = match msg.role {
            ChatRole::User => ("user", "input_text"),
            ChatRole::Assistant => ("assistant", "output_text"),
        };
        match &msg.message_type {
            MessageType::Text => {
                inputs.push(CodexInputItem::Message {
                    role,
                    content: vec![CodexInputContent {
                        content_type,
                        text: &msg.content,
                    }],
                });
            }
            MessageType::ToolUse(calls) => {
                if !msg.content.trim().is_empty() {
                    inputs.push(CodexInputItem::Message {
                        role,
                        content: vec![CodexInputContent {
                            content_type,
                            text: &msg.content,
                        }],
                    });
                }
                for call in calls {
                    inputs.push(CodexInputItem::FunctionCall {
                        call_id: &call.id,
                        name: &call.function.name,
                        arguments: &call.function.arguments,
                    });
                }
            }
            MessageType::ToolResult(results) => {
                for result in results {
                    inputs.push(CodexInputItem::FunctionCallOutput {
                        call_id: &result.id,
                        output: &result.function.arguments,
                    });
                }
            }
            _ => {
                return Err(LLMError::ProviderError(
                    "Codex backend only supports text messages".to_string(),
                ));
            }
        }
    }

    let request_tools = tools
        .map(to_codex_tools)
        .or_else(|| cfg.tools().map(to_codex_tools));
    let request_tool_choice = if request_tools.is_some() {
        cfg.tool_choice().cloned()
    } else {
        None
    };

    let body = CodexChatRequest {
        model: cfg.model(),
        input: inputs,
        instructions,
        store: false,
        max_output_tokens: cfg.max_tokens().copied(),
        temperature: cfg.temperature().copied(),
        stream: true,
        top_p: cfg.top_p().copied(),
        top_k: cfg.top_k().copied(),
        tools: request_tools,
        tool_choice: request_tool_choice,
    };

    let json_body = serde_json::to_vec(&body)?;
    let url = cfg
        .base_url()
        .join("responses")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;
    let api_key = cfg.api_key();
    let account_id = chatgpt_account_id(api_key)?;

    Ok(Request::builder()
        .method(Method::POST)
        .uri(url.to_string())
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header("ChatGPT-Account-ID", account_id)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "codex_cli_rs")
        .header(CONTENT_TYPE, "application/json")
        .body(json_body)?)
}

fn to_codex_tools(tools: &[Tool]) -> Vec<CodexTool<'_>> {
    tools
        .iter()
        .map(|tool| CodexTool {
            tool_type: tool.tool_type.as_str(),
            name: tool.function.name.as_str(),
            description: tool.function.description.as_str(),
            parameters: &tool.function.parameters,
            strict: false,
        })
        .collect()
}

pub fn codex_parse_chat(response: Response<Vec<u8>>) -> Result<Box<dyn ChatResponse>, LLMError> {
    handle_http_error!(response);

    let json_resp: Result<CodexChatResponse, serde_json::Error> =
        serde_json::from_slice(&response.body());
    match json_resp {
        Ok(response) => Ok(Box::new(response)),
        Err(e) => Err(LLMError::ResponseFormatError {
            message: format!("Failed to decode API response: {}", e),
            raw_response: String::new(),
        }),
    }
}

pub fn codex_parse_stream_chunk_with_state(
    chunk: &[u8],
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) -> Result<Vec<StreamChunk>, LLMError> {
    if chunk.is_empty() {
        return Ok(Vec::new());
    }

    let text = String::from_utf8_lossy(chunk);
    let mut results = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let data = match line.strip_prefix("data: ") {
            Some(data) => data,
            None => continue,
        };

        if data == "[DONE]" {
            results.push(StreamChunk::Done {
                stop_reason: "end_turn".to_string(),
            });
            continue;
        }

        let event: CodexSseEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(_) => continue,
        };

        match event.kind.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = event.delta {
                    results.push(StreamChunk::Text(delta));
                }
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = event.item {
                    handle_output_item_event(
                        &item,
                        event.output_index,
                        &mut results,
                        tool_state_buffer,
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.delta {
                    handle_function_call_arguments_delta(
                        event.output_index,
                        event.item_id.as_deref(),
                        &delta,
                        &mut results,
                        tool_state_buffer,
                    );
                }
            }
            "response.function_call_arguments.done" => {
                handle_function_call_arguments_done(
                    event.output_index,
                    event.item_id.as_deref(),
                    &mut results,
                    tool_state_buffer,
                );
            }
            "response.completed" => {
                if let Some(response) = event.response {
                    emit_tool_calls_from_response(&response, &mut results, tool_state_buffer);
                    if let Some(usage_value) = response.get("usage") {
                        if let Ok(usage) = serde_json::from_value::<Usage>(usage_value.clone()) {
                            results.push(StreamChunk::Usage(usage));
                        }
                    }
                }
                results.push(StreamChunk::Done {
                    stop_reason: "end_turn".to_string(),
                });
            }
            "response.failed" => {
                let message = event
                    .response
                    .as_ref()
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex response failed");
                return Err(LLMError::ProviderError(message.to_string()));
            }
            _ => {}
        }
    }

    Ok(results)
}

fn handle_output_item_event(
    item: &Value,
    output_index: Option<usize>,
    results: &mut Vec<StreamChunk>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    if item_type != "function_call" {
        return;
    }

    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = item.get("name").and_then(Value::as_str).map(str::to_string);
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .map(str::to_string);

    let index = resolve_tool_index(output_index, tool_state_buffer);
    let mut buffer = tool_state_buffer.lock().unwrap();
    let state = buffer.entry(index).or_default();
    if let Some(id) = id {
        state.id = Some(id);
    }
    if let Some(name) = name {
        state.name = Some(name);
    }
    if !state.started {
        if let (Some(id), Some(name)) = (state.id.clone(), state.name.clone()) {
            state.started = true;
            results.push(StreamChunk::ToolUseStart { index, id, name });
        }
    }

    if let Some(arguments) = arguments {
        emit_arguments_delta(index, &arguments, state, results);
        emit_tool_complete(index, state, results);
    }
}

fn handle_function_call_arguments_delta(
    output_index: Option<usize>,
    item_id: Option<&str>,
    delta: &str,
    results: &mut Vec<StreamChunk>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) {
    let index = resolve_tool_index_with_item(output_index, item_id, tool_state_buffer);
    let mut buffer = tool_state_buffer.lock().unwrap();
    let state = buffer.entry(index).or_default();
    if !state.started {
        if let (Some(id), Some(name)) = (state.id.clone(), state.name.clone()) {
            state.started = true;
            results.push(StreamChunk::ToolUseStart { index, id, name });
        }
    }
    state.arguments.push_str(delta);
    results.push(StreamChunk::ToolUseInputDelta {
        index,
        partial_json: delta.to_string(),
    });
}

fn handle_function_call_arguments_done(
    output_index: Option<usize>,
    item_id: Option<&str>,
    results: &mut Vec<StreamChunk>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) {
    let index = resolve_tool_index_with_item(output_index, item_id, tool_state_buffer);
    let mut buffer = tool_state_buffer.lock().unwrap();
    if let Some(state) = buffer.get_mut(&index) {
        if !state.started {
            if let (Some(id), Some(name)) = (state.id.clone(), state.name.clone()) {
                state.started = true;
                results.push(StreamChunk::ToolUseStart { index, id, name });
            }
        }
        emit_tool_complete(index, state, results);
    }
}

fn emit_tool_calls_from_response(
    response: &Value,
    results: &mut Vec<StreamChunk>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) {
    let Some(items) = response.get("output").and_then(Value::as_array) else {
        return;
    };

    for (idx, item) in items.iter().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if item_type != "function_call" {
            continue;
        }

        let id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let name = item.get("name").and_then(Value::as_str).map(str::to_string);
        let arguments = item.get("arguments").and_then(Value::as_str).unwrap_or("");

        let index = resolve_tool_index(Some(idx), tool_state_buffer);
        let mut buffer = tool_state_buffer.lock().unwrap();
        let state = buffer.entry(index).or_default();
        if let Some(id) = id {
            state.id = Some(id);
        }
        if let Some(name) = name {
            state.name = Some(name);
        }
        if !state.started {
            if let (Some(id), Some(name)) = (state.id.clone(), state.name.clone()) {
                state.started = true;
                results.push(StreamChunk::ToolUseStart { index, id, name });
            }
        }
        if !arguments.is_empty() {
            emit_arguments_delta(index, arguments, state, results);
        }
        emit_tool_complete(index, state, results);
    }
}

fn emit_tool_complete(index: usize, state: &CodexToolUseState, results: &mut Vec<StreamChunk>) {
    if let (Some(id), Some(name)) = (state.id.clone(), state.name.clone()) {
        results.push(StreamChunk::ToolUseComplete {
            index,
            tool_call: ToolCall {
                id,
                call_type: "function".to_string(),
                function: FunctionCall {
                    name,
                    arguments: state.arguments.clone(),
                },
            },
        });
    }
}

fn emit_arguments_delta(
    index: usize,
    arguments: &str,
    state: &mut CodexToolUseState,
    results: &mut Vec<StreamChunk>,
) {
    if arguments.starts_with(&state.arguments) {
        let delta = &arguments[state.arguments.len()..];
        if !delta.is_empty() {
            state.arguments.push_str(delta);
            results.push(StreamChunk::ToolUseInputDelta {
                index,
                partial_json: delta.to_string(),
            });
        }
    } else {
        state.arguments = arguments.to_string();
        results.push(StreamChunk::ToolUseInputDelta {
            index,
            partial_json: arguments.to_string(),
        });
    }
}

fn resolve_tool_index(
    output_index: Option<usize>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) -> usize {
    if let Some(index) = output_index {
        return index;
    }
    let buffer = tool_state_buffer.lock().unwrap();
    let mut index = 0;
    while buffer.contains_key(&index) {
        index += 1;
    }
    index
}

fn resolve_tool_index_with_item(
    output_index: Option<usize>,
    item_id: Option<&str>,
    tool_state_buffer: &Arc<Mutex<HashMap<usize, CodexToolUseState>>>,
) -> usize {
    if let Some(index) = output_index {
        return index;
    }
    if let Some(item_id) = item_id {
        let buffer = tool_state_buffer.lock().unwrap();
        if let Some((index, _)) = buffer
            .iter()
            .find(|(_, state)| state.id.as_deref() == Some(item_id))
        {
            return *index;
        }
    }
    resolve_tool_index(None, tool_state_buffer)
}

pub fn codex_list_models_request<C: CodexProviderConfig>(
    cfg: &C,
) -> Result<Request<Vec<u8>>, LLMError> {
    let mut url = cfg
        .base_url()
        .join("models")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;
    let client_version = cfg.client_version().unwrap_or(env!("CARGO_PKG_VERSION"));
    url.query_pairs_mut()
        .append_pair("client_version", client_version);

    let api_key = cfg.api_key();
    let account_id = chatgpt_account_id(api_key)?;

    Ok(Request::builder()
        .method(Method::GET)
        .uri(url.to_string())
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header("ChatGPT-Account-ID", account_id)
        .body(Vec::new())?)
}

pub fn codex_list_models_request_from_value(cfg: &Value) -> Result<Request<Vec<u8>>, LLMError> {
    let api_key = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .ok_or_else(|| LLMError::InvalidRequest("Could not find api_key".to_string()))?;
    let base_url = cfg
        .get("base_url")
        .and_then(Value::as_str)
        .map(Url::parse)
        .transpose()
        .map_err(|e| LLMError::HttpError(e.to_string()))?
        .unwrap_or_else(|| Url::parse("https://chatgpt.com/backend-api/codex/").unwrap());
    let client_version = cfg
        .get("client_version")
        .and_then(Value::as_str)
        .unwrap_or(env!("CARGO_PKG_VERSION"));

    let mut url = base_url
        .join("models")
        .map_err(|e| LLMError::HttpError(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_version", client_version);

    let account_id = chatgpt_account_id(api_key)?;

    Ok(Request::builder()
        .method(Method::GET)
        .uri(url.to_string())
        .header(AUTHORIZATION, format!("Bearer {}", api_key))
        .header("ChatGPT-Account-ID", account_id)
        .body(Vec::new())?)
}

pub fn codex_parse_list_models(response: Response<Vec<u8>>) -> Result<Vec<String>, LLMError> {
    handle_http_error!(response);
    let json_resp: Result<CodexModelsResponse, serde_json::Error> =
        serde_json::from_slice(&response.body());
    match json_resp {
        Ok(response) => Ok(response.models.into_iter().map(|m| m.slug).collect()),
        Err(e) => Err(LLMError::ResponseFormatError {
            message: format!("Failed to decode models response: {}", e),
            raw_response: String::new(),
        }),
    }
}

fn chatgpt_account_id(access_token: &str) -> Result<String, LLMError> {
    let payload = access_token
        .split('.')
        .nth(1)
        .ok_or_else(|| LLMError::InvalidRequest("Invalid OAuth access token".to_string()))?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).map_err(|e| {
        LLMError::InvalidRequest(format!("Invalid OAuth access token payload: {}", e))
    })?;
    let payload_value: Value = serde_json::from_slice(&decoded).map_err(|e| {
        LLMError::InvalidRequest(format!("Invalid OAuth access token payload JSON: {}", e))
    })?;
    payload_value
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            LLMError::InvalidRequest("OAuth access token is missing chatgpt_account_id".to_string())
        })
}

fn resolve_instructions<'a>(
    model: &str,
    override_text: Option<&'a str>,
) -> Result<&'a str, LLMError> {
    if let Some(text) = override_text.filter(|text| !text.trim().is_empty()) {
        return Ok(text);
    }

    let instructions = match model {
        "gpt-5.1-codex-max" | "bengalfox" => INSTRUCTIONS_GPT_5_1_CODEX_MAX,
        "gpt-5.1-codex" | "gpt-5.1-codex-mini" | "gpt-5-codex" | "gpt-5-codex-mini"
        | "codex-mini-latest" => INSTRUCTIONS_GPT_5_1_CODEX,
        "gpt-5.2" | "gpt-5.2-codex" | "boomslang" => INSTRUCTIONS_GPT_5_2,
        "gpt-5.1" => INSTRUCTIONS_GPT_5_1,
        "gpt-5" => INSTRUCTIONS_GPT_5,
        _ => INSTRUCTIONS_GPT_5_1_CODEX,
    };

    Ok(instructions)
}
